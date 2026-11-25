use super::*;

/// I1, as production runs it (root, uids 10001 and 10002): a Codex agent
/// and a concurrent Claude answer run cannot read each other's process
/// environment, home or workspace, and a process the agent started in a
/// session of its own dies with the agent run, as does what it left in
/// `/tmp`. Run as root in a container with the internal-work service's
/// capabilities (see the C2 report).
#[tokio::test]
#[ignore = "needs root: run in a container as the C2 report says"]
async fn as_root_agent_and_answer_runs_cannot_reach_each_other_and_nothing_of_the_agent_outlives_it()
 {
    let probe = |me: &str, other: &str| {
        format!(
            "id -u > \"$rec/{me}.uid\"; stat -c %u . > \"$rec/{me}.owner\"\n\
                 echo $$ > \"$rec/{me}.pid\"; echo \"$HOME\" > \"$rec/{me}.homepath\"\n\
                 pwd -P > \"$rec/{me}.cwd\"\n\
                 chmod 644 \"$rec/{me}.pid\" \"$rec/{me}.homepath\" \"$rec/{me}.cwd\"\n\
                 {}\n\
                 p=$(cat \"$rec/{other}.pid\"); h=$(cat \"$rec/{other}.homepath\"); w=$(cat \"$rec/{other}.cwd\")\n\
                 cat /proc/$p/environ > /dev/null 2>&1 && echo environ >> \"$rec/{me}.reached\"\n\
                 ls \"$h\" > /dev/null 2>&1 && echo home >> \"$rec/{me}.reached\"\n\
                 cat \"$h/auth.json\" > /dev/null 2>&1 && echo login >> \"$rec/{me}.reached\"\n\
                 ls \"$w\" > /dev/null 2>&1 && echo workspace >> \"$rec/{me}.reached\"\n\
                 cat \"$rec/{other}.env\" > /dev/null 2>&1 && echo record >> \"$rec/{me}.reached\"\n\
                 touch \"$rec/{me}.checked\"; chmod 644 \"$rec/{me}.checked\"\n{}",
            wait_for(&format!("{other}.pid")),
            wait_for(&format!("{other}.checked")),
        )
    };
    let codex = format!(
        "cat /proc/$$/environ > \"$rec/codex.env\"\n\
             setsid sleep 30 &\necho $! > \"$rec/escaped\"\ntouch /tmp/agent-left-behind\n{}\n{}",
        probe("codex", "claude"),
        saving("codex")
    );
    let claude = format!(
        "{}\n{}",
        probe("claude", "codex"),
        claude_printing(&claude_result(false, ANSWER), 0)
    );
    let f = Fixture::as_root(&codex, &claude).await;
    let answer = f.submit(&[claude_candidate()]).await;
    let agent = f
        .submit_request(&agent_request(), &[codex_candidate()])
        .await;
    let host = serving(&f);
    let answer = answered_within(&f, answer, 20).await;
    let agent = answered_within(&f, agent, 20).await;
    let left = none_left_of(10002).await;
    host.abort();
    assert_eq!(answer.attempts.len(), 1);
    assert_eq!(agent.attempts.len(), 1);
    for (me, uid) in [("codex", "10002"), ("claude", "10001")] {
        let read = |name: &str| std::fs::read_to_string(f.rec(&format!("{me}.{name}")));
        assert!(
            read("reached").is_err(),
            "{me} reached {:?}",
            read("reached")
        );
        assert_eq!(read("uid").unwrap().trim(), uid, "{me}");
        assert_eq!(read("owner").unwrap().trim(), uid, "{me} workspace owner");
    }
    assert!(left.is_empty(), "agent processes left: {left:?}");
    // It had started a process in a session of its own; none is left.
    assert!(f.rec("escaped").exists());
    assert!(!Path::new("/tmp/agent-left-behind").exists());
}

/// I2, as production runs it: a Claude agent (uid 10002) cannot change
/// its settings or its shims, nor add to the directory holding them; its
/// workspace is its own.
#[tokio::test]
#[ignore = "needs root: run in a container as the C2 report says"]
async fn as_root_a_claude_agent_cannot_change_its_settings_or_shims() {
    let claude = format!(
        "r=$(dirname \"$HOME\")\n\
             echo x >> \"$r/settings.json\" 2>/dev/null && echo write >> \"$rec/b.changed\"\n\
             mv \"$r/settings.json\" \"$r/moved\" 2>/dev/null && echo rename >> \"$rec/b.changed\"\n\
             echo x >> \"$r/bin/aq-playbooks\" 2>/dev/null && echo shim >> \"$rec/b.changed\"\n\
             touch \"$r/bin/other\" 2>/dev/null && echo bin >> \"$rec/b.changed\"\n\
             touch \"$r/other\" 2>/dev/null && echo root >> \"$rec/b.changed\"\n\
             id -u > \"$rec/b.uid\"; stat -c %u doc.md > \"$rec/b.owner\"\n\
             stat -c %u \"$r/settings.json\" > \"$rec/b.settings\"\n{}",
        saving("claude")
    );
    let f = Fixture::as_root("exit 1", &claude).await;
    let id = f
        .submit_request(&agent_request(), &[claude_candidate()])
        .await;
    assert!(f.tick().await);
    assert_eq!(f.row(id).await.status, "answered");
    let read = |name: &str| std::fs::read_to_string(f.rec(name));
    assert!(
        read("b.changed").is_err(),
        "changed: {:?}",
        read("b.changed")
    );
    assert_eq!(read("b.uid").unwrap().trim(), "10002");
    assert_eq!(read("b.owner").unwrap().trim(), "10002");
    assert_eq!(read("b.settings").unwrap().trim(), "0");
}
