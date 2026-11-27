use super::*;

struct InputWindow {
    bytes: usize,
    painting: bool,
    fixed: bool,
}

enum Frame {
    Forward(Value),
    Ignore,
    Disconnect,
}

impl InputWindow {
    fn new(fixed: bool) -> Self {
        Self {
            bytes: 0,
            painting: fixed,
            fixed,
        }
    }
    fn observe_output(&mut self, message: &Message) {
        if matches!(message, Message::Text(_)) {
            self.painting = false;
        }
    }
    fn message(&mut self, frame: Message, read_only: bool) -> Frame {
        let (payload, charge) = match frame {
            Message::Binary(bytes) if !read_only && self.allowed(&bytes) => {
                (json!({"kind":"input","bytes":bytes.to_vec()}), bytes.len())
            }
            Message::Text(text) => match browser_control(&text, self.fixed, read_only) {
                Some(command) => command,
                None => return Frame::Disconnect,
            },
            Message::Ping(_) | Message::Pong(_) => return Frame::Ignore,
            _ => return Frame::Disconnect,
        };
        self.charge(payload, charge)
    }
    fn charge(&mut self, payload: Value, charge: usize) -> Frame {
        if self.painting {
            return Frame::Ignore;
        }
        self.painting = payload["kind"] == "repaint";
        self.bytes += charge;
        if self.bytes > 65536 {
            return Frame::Disconnect;
        }
        Frame::Forward(payload)
    }
    fn allowed(&self, bytes: &[u8]) -> bool {
        !bytes.is_empty()
            && bytes.len() <= MAX_INPUT_BYTES
            && (!self.fixed || complete_terminal_input(bytes, false))
    }
}

impl Browser {
    pub(super) async fn run(self, mut output: mpsc::Receiver<Message>, socket: WebSocket) {
        let (mut sink, mut source) = socket.split();
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        let fixed = self.s.subject == Subject::Attempt;
        let mut input = InputWindow::new(fixed);
        loop {
            let connected = tokio::select! {
                message=output.recv()=>match message {
                    Some(message)=>{input.observe_output(&message); send(&mut sink,message).await},
                    None=>false,
                },
                frame=source.next()=>match frame {
                    Some(Ok(frame))=>self.input(&mut input,frame).await,
                    _=>false,
                },
                _=tick.tick()=>{input.bytes=0; self.renew().await && send(&mut sink,Message::Ping(Vec::new().into())).await},
            };
            if !connected {
                break;
            }
        }
        self.detach().await;
    }
    async fn input(&self, window: &mut InputWindow, frame: Message) -> bool {
        match window.message(frame, self.a.read_only) {
            Frame::Forward(payload) => self.forward(payload).await,
            Frame::Ignore => true,
            Frame::Disconnect => false,
        }
    }
    fn attached(&self, screen: &terminal::Screen) -> bool {
        controls(screen, self.actor, &self.a)
            && screen
                .viewer
                .as_ref()
                .is_some_and(|v| v.connection == self.connection)
            && screen.relay.as_ref().is_some_and(|r| r.id == self.relay.id)
    }
    async fn forward(&self, mut payload: Value) -> bool {
        let screens = self.s.runtime.0.lock().await;
        if !screens.get(&self.id).is_some_and(|s| self.attached(s)) {
            return false;
        }
        payload["connection_id"] = json!(self.connection);
        payload["generation"] = json!(self.a.generation);
        self.relay.tx.try_send(payload).is_ok()
    }
    async fn renew(&self) -> bool {
        if (self.s.owner)(self.headers.clone()).await.ok() != Some(self.actor)
            || !permitted(&self.s, self.id).await
        {
            return false;
        }
        let mut screens = self.s.runtime.0.lock().await;
        let Some(screen) = screens.get_mut(&self.id).filter(|s| self.attached(s)) else {
            return false;
        };
        screen.controller = Some((self.a.client_id, self.actor, now() + 15000));
        drop(screens);
        if !self.a.read_only {
            attempt::controlled(&self.s, self.id, self.actor).await;
        }
        true
    }
    async fn detach(&self) {
        let mut screens = self.s.runtime.0.lock().await;
        let Some(screen) = screens.get_mut(&self.id).filter(|s| {
            s.viewer
                .as_ref()
                .is_some_and(|v| v.connection == self.connection)
        }) else {
            return;
        };
        screen.viewer = None;
        let _ = self.relay.tx.try_send(
            json!({"kind":"detach","connection_id":self.connection,"generation":self.a.generation}),
        );
        drop(screens);
        if !self.a.read_only {
            attempt::released(&self.s, self.id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repaint_pauses_valid_input_until_the_worker_announces_readiness() {
        let mut window = InputWindow::new(true);
        let repaint = Message::Text(r#"{"kind":"repaint"}"#.into());
        let input = Message::Binary(vec![b'x'].into());
        let ready = Message::Text(r#"{"kind":"ready"}"#.into());
        assert!(matches!(
            window.message(input.clone(), false),
            Frame::Ignore
        ));
        window.observe_output(&input);
        assert!(matches!(
            window.message(repaint.clone(), true),
            Frame::Ignore
        ));
        assert!(matches!(
            window.message(input.clone(), true),
            Frame::Disconnect
        ));
        window.observe_output(&ready);
        assert!(matches!(window.message(repaint, true), Frame::Forward(_)));
        assert!(matches!(
            window.message(input.clone(), false),
            Frame::Ignore
        ));
        assert_eq!(window.bytes, 0);
        window.observe_output(&ready);
        assert!(matches!(window.message(input, false), Frame::Forward(_)));
        assert_eq!(window.bytes, 1);
    }

    #[test]
    fn accepts_exactly_the_binary_input_budget() {
        let mut window = InputWindow::new(false);
        let chunk = Message::Binary(vec![b'x'; MAX_INPUT_BYTES].into());
        for _ in 0..65536 / MAX_INPUT_BYTES {
            assert!(matches!(
                window.message(chunk.clone(), false),
                Frame::Forward(_)
            ));
        }
        assert!(matches!(
            window.message(Message::Ping(vec![1].into()), false),
            Frame::Ignore
        ));
        let byte = Message::Binary(vec![b'x'].into());
        assert!(matches!(window.message(byte, false), Frame::Disconnect));
    }
}
