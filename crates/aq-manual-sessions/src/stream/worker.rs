use super::*;

pub(super) struct Worker {
    pub(super) service: Service,
    pub(super) headers: HeaderMap,
    pub(super) session: Uuid,
    pub(super) node: Uuid,
    pub(super) relay_id: Uuid,
}

impl Worker {
    pub(super) async fn run(self, mut commands: mpsc::Receiver<Value>, socket: WebSocket) {
        let (mut sink, mut source) = socket.split();
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.tick().await;
        loop {
            let connected = tokio::select! {
                command=commands.recv()=>match command {
                    Some(command)=>send(&mut sink,Message::Text(command.to_string().into())).await,
                    None=>false,
                },
                frame=source.next()=>match frame {
                    Some(Ok(frame))=>self.forward(frame).await,
                    _=>false,
                },
                _=tick.tick()=>self.authorized().await && send(&mut sink,Message::Ping(Vec::new().into())).await,
            };
            if !connected {
                break;
            }
        }
        self.detach().await;
    }
    async fn detach(&self) {
        let mut screens = self.service.runtime.0.lock().await;
        if let Some(screen) = screens.get_mut(&self.session).filter(|s| self.owns(s)) {
            screen.relay = None;
            screen.viewer = None;
        }
    }

    fn owns(&self, screen: &terminal::Screen) -> bool {
        screen
            .relay
            .as_ref()
            .is_some_and(|relay| relay.id == self.relay_id)
    }

    async fn authorized(&self) -> bool {
        if (self.service.node)(self.headers.clone()).await.ok() != Some(self.node)
            || !permitted(&self.service, self.session).await
        {
            return false;
        }
        let screens = self.service.runtime.0.lock().await;
        screens.get(&self.session).is_some_and(|s| self.owns(s))
    }

    async fn forward(&self, frame: Message) -> bool {
        let mut screens = self.service.runtime.0.lock().await;
        let Some(screen) = screens.get_mut(&self.session).filter(|s| self.owns(s)) else {
            return false;
        };
        let live_controller = screen.controller.is_some_and(|(_, _, until)| until > now());
        let Some(viewer) = screen
            .viewer
            .as_ref()
            .filter(|v| v.generation == screen.generation && live_controller)
        else {
            return true;
        };
        if matches!(frame, Message::Close(_)) {
            return false;
        }
        let output = viewer.output(frame, self.service.subject == Subject::Attempt);
        if output.is_some_and(|message| viewer.tx.try_send(message).is_err()) {
            let connection = viewer.connection;
            let generation = viewer.generation;
            screen.viewer = None;
            if let Some(relay) = &screen.relay {
                let _ = relay.tx.try_send(
                    json!({"kind":"detach","connection_id":connection,"generation":generation}),
                );
            }
        }
        true
    }
}

impl Viewer {
    fn output(&self, frame: Message, fixed: bool) -> Option<Message> {
        match frame {
            Message::Binary(bytes)
                if (17..=16400).contains(&bytes.len())
                    && bytes[..16] == *self.connection.as_bytes() =>
            {
                Some(Message::Binary(bytes.slice(16..)))
            }
            Message::Text(text) => ready(&text, self.connection, fixed),
            _ => None,
        }
    }
}
