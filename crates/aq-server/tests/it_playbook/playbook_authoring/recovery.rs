use super::*;

#[tokio::test]
async fn a_run_stopped_between_its_task_id_and_submit_is_submitted_once() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.document("routing").set(POLICY, 1, options());
    let run = requested(&host, "routing").await.unwrap().unwrap();
    // The server gives the run its task id, then stops before submitting.
    let task = task_for(&pool, run).await.unwrap().unwrap();
    assert_eq!(tasks(&pool, task).await, 0);

    // After the restart the run is in flight with that id; two resumers race.
    let listed = in_flight(&pool).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|r| (r.id, r.internal_task_id))
            .collect::<Vec<_>>(),
        vec![(run, Some(task))]
    );
    let (one, two) = tokio::join!(task_for(&pool, run), task_for(&pool, run));
    assert_eq!((one.unwrap(), two.unwrap()), (Some(task), Some(task)));
    let request = authoring_request(run);
    let none: &[aq_internal_work::Candidate] = &[];
    let (one, two) = tokio::join!(
        internal::Queue::at(&pool, 1_000).submit_as(task, &request, Ok(none)),
        internal::Queue::at(&pool, 2_000).submit_as(task, &request, Ok(none)),
    );
    assert_eq!((one.unwrap(), two.unwrap()), (task, task));
    assert_eq!(tasks(&pool, task).await, 1);
    // A run no longer running gets no task id.
    assert!(finish(&pool, run, &Finished::default()).await.unwrap());
    assert_eq!(task_for(&pool, run).await.unwrap(), None);
}

#[tokio::test]
async fn a_task_finished_while_the_server_was_down_is_recorded_once() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.document("routing").set(POLICY, 1, options());
    let run = requested(&host, "routing").await.unwrap().unwrap();
    let task = task_for(&pool, run).await.unwrap().unwrap();
    // The task ends (here: no candidate) while nobody waits on it.
    internal::Queue::at(&pool, 1_000)
        .submit_as(task, &authoring_request(run), Err("no profile"))
        .await
        .unwrap();
    let row = internal::get(&pool, task).await.unwrap().unwrap();
    assert_eq!(row.status, "failed");

    // Two resumers find the run in flight and record its result: once.
    assert_eq!(in_flight(&pool).await.unwrap().len(), 1);
    let failed = Finished {
        refusal: Some("no profile".into()),
        ..Finished::default()
    };
    let (one, two) = tokio::join!(finish(&pool, run, &failed), finish(&pool, run, &failed));
    let recorded = [one.unwrap(), two.unwrap()];
    assert_eq!(recorded.iter().filter(|r| **r).count(), 1, "{recorded:?}");
    assert!(in_flight(&pool).await.unwrap().is_empty());
    let (ended, finished) = status(&pool, run).await;
    assert_eq!(ended, "failed");
    assert!(finished.is_some());
}
