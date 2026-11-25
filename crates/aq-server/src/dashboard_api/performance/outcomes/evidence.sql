WITH clocked_completions AS NOT MATERIALIZED (
    SELECT p.project_id, s.task_id, s.updated_at_ms AS at_ms
    FROM profiled_auto_settlements s JOIN result_revision_pins p USING (report_id)
    WHERE s.disposition='confirmed' AND s.task_id=p.task_id
      AND ($1::uuid IS NULL OR p.project_id=$1)
    UNION ALL
    SELECT (l.execution_spec#>>'{lifecycle,project_id}')::uuid, c.task_id, t.confirmed_at_ms
    FROM performance_pilot_completion_times t
    JOIN profiled_task_completions c USING (operation_id)
    JOIN profiled_launches l USING (attempt_id)
    WHERE ($1 IS NULL OR (l.execution_spec#>>'{lifecycle,project_id}')::uuid=$1)
), recent_tasks AS (
    SELECT project_id, task_id FROM clocked_completions WHERE at_ms>$2 AND at_ms<=$3
    UNION
    SELECT project_id, task_id FROM delivery_merge_evidence
    WHERE observed_at_ms>$2 AND observed_at_ms<=$3 AND ($1 IS NULL OR project_id=$1)
    UNION
    SELECT project_id, task_id FROM result_revision_pins
    WHERE submitted_at_ms>$2 AND submitted_at_ms<=$3 AND ($1 IS NULL OR project_id=$1)
    UNION
    SELECT p.project_id, p.task_id FROM delivery_decisions d JOIN result_revision_pins p
        ON p.report_id=d.result_id AND p.task_id=d.task_id
    WHERE d.decided_at_ms>$2 AND d.decided_at_ms<=$3 AND ($1 IS NULL OR p.project_id=$1)
    UNION
    SELECT p.project_id,p.task_id FROM profiled_auto_settlements s JOIN result_revision_pins p USING (report_id)
    WHERE s.updated_at_ms>$2 AND s.updated_at_ms<=$3 AND ($1 IS NULL OR p.project_id=$1)
    UNION
    SELECT p.project_id,p.task_id FROM profiled_settlement_evaluations s JOIN result_revision_pins p USING (report_id)
    WHERE s.created_at_ms>$2 AND s.created_at_ms<=$3 AND ($1 IS NULL OR p.project_id=$1)
    UNION
    SELECT (l.execution_spec#>>'{lifecycle,project_id}')::uuid,a.task_id
    FROM profiled_cancelled_settlements c JOIN profiled_launches l USING(attempt_id)
    JOIN execution_attempts a USING(attempt_id)
    WHERE c.recorded_at_ms>$2 AND c.recorded_at_ms<=$3
      AND ($1 IS NULL OR (l.execution_spec#>>'{lifecycle,project_id}')::uuid=$1)
), scoped_tasks AS (
    SELECT project_id,task_id FROM recent_tasks WHERE $4::uuid IS NULL OR task_id=$4
    UNION
    SELECT project_id,task_id FROM performance_task_first_ready
    WHERE task_id=$4 AND ($1 IS NULL OR project_id=$1) AND ready_at_ms<=$3
    UNION
    SELECT project_id,task_id FROM result_revision_pins
    WHERE task_id=$4 AND ($1 IS NULL OR project_id=$1) AND submitted_at_ms<=$3
    UNION
    SELECT project_id,task_id FROM delivery_merge_evidence
    WHERE task_id=$4 AND ($1 IS NULL OR project_id=$1) AND observed_at_ms<=$3
), cohorts AS (
    SELECT s.project_id,s.task_id,r.ready_at_ms,r.work_class,
        COALESCE(override.definition, defaults.definition) AS definition,
        CASE WHEN override.definition IS NOT NULL THEN override.revision
             WHEN defaults.definition IS NOT NULL THEN defaults.revision END AS definition_revision,
        CASE WHEN override.definition IS NOT NULL THEN r.work_class
             WHEN defaults.definition IS NOT NULL THEN '' END AS definition_class
    FROM scoped_tasks s
    LEFT JOIN performance_task_first_ready r
      ON r.project_id=s.project_id AND r.task_id=s.task_id AND r.ready_at_ms<=$3
    LEFT JOIN LATERAL (
        SELECT revision, definition FROM performance_delivery_definitions d
        WHERE d.project_id=s.project_id AND d.work_class=r.work_class AND d.effective_at_ms<=r.ready_at_ms
        ORDER BY d.revision DESC LIMIT 1
    ) override ON true
    LEFT JOIN LATERAL (
        SELECT revision, definition FROM performance_delivery_definitions d
        WHERE d.project_id=s.project_id AND d.work_class='' AND d.effective_at_ms<=r.ready_at_ms
        ORDER BY d.revision DESC LIMIT 1
    ) defaults ON true
), merges AS (
    SELECT DISTINCT ON (m.project_id,m.task_id,repository,m.pull_request)
        m.project_id,m.task_id,
        COALESCE(
            lower(m.target#>>'{github,owner}') || '/' || lower(m.target#>>'{github,name}'),
            m.target#>>'{git,repository_id}'
        ) AS repository,
        m.target#>>'{git,repository_id}' AS repository_id,m.target_ref,
        m.pull_request,m.observed_at_ms AS at_ms
    FROM scoped_tasks s JOIN delivery_merge_evidence m USING (project_id,task_id)
    WHERE m.observed_at_ms<=$3
    ORDER BY m.project_id,m.task_id,repository,m.pull_request,m.observed_at_ms,m.operation_id
), first_merge_observations AS (
    SELECT repository,pull_request,min(at_ms) AS at_ms
    FROM merges m
    WHERE repository IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM delivery_merge_evidence prior
        WHERE prior.pull_request=m.pull_request AND prior.observed_at_ms<=$2
          AND ($1 IS NULL OR prior.project_id=$1)
          AND COALESCE(
              lower(prior.target#>>'{github,owner}') || '/' || lower(prior.target#>>'{github,name}'),
              prior.target#>>'{git,repository_id}'
          )=m.repository
    )
    GROUP BY repository,pull_request
), merge_events AS (
    SELECT first.repository,first.pull_request,
           min(m.at_ms-c.ready_at_ms) FILTER (WHERE m.at_ms>=c.ready_at_ms) AS elapsed
    FROM first_merge_observations first
    JOIN merges m USING (repository,pull_request,at_ms)
    JOIN cohorts c ON c.project_id=m.project_id AND c.task_id=m.task_id
    GROUP BY first.repository,first.pull_request
), task_merge_clocks AS (
    SELECT m.*, COALESCE(first.at_ms,m.at_ms) AS verified_at_ms
    FROM merges m
    LEFT JOIN LATERAL (
        SELECT min(prior.observed_at_ms) AS at_ms
        FROM delivery_merge_evidence prior
        WHERE prior.project_id=m.project_id AND prior.pull_request=m.pull_request
          AND prior.observed_at_ms<=m.at_ms
          AND COALESCE(
              lower(prior.target#>>'{github,owner}') || '/' || lower(prior.target#>>'{github,name}'),
              prior.target#>>'{git,repository_id}'
          )=m.repository
    ) first ON true
), tasks AS (
    SELECT c.*,
        completion.at_ms AS completion_at_ms,
        merge.at_ms AS merge_at_ms,
        CASE
            WHEN c.definition->>'kind'='confirmed_completion' AND completion.at_ms>=c.ready_at_ms
                THEN completion.at_ms
            WHEN milestones.required_candidates=1 AND milestones.verified_at_ms>=c.ready_at_ms
                THEN milestones.verified_at_ms
        END AS delivery_at_ms,
        CASE WHEN milestones.required_candidates>1 THEN 'required_pr_set_unknown' END AS delivery_evidence,
        first_report.submitted_at_ms,
        decision.outcome AS first_pass,
        CASE WHEN decision.outcome IS NOT NULL THEN decision.at_ms END AS adjudicated_at_ms,
        COALESCE(last_decision.outcome, 'undetermined') AS disposition
    FROM cohorts c
    LEFT JOIN LATERAL (
        SELECT min(at_ms) AS at_ms FROM clocked_completions s
        WHERE s.project_id=c.project_id AND s.task_id=c.task_id AND at_ms<=$3
    ) completion ON true
    LEFT JOIN LATERAL (
        SELECT min(verified_at_ms) AS at_ms
        FROM task_merge_clocks m WHERE m.project_id=c.project_id AND m.task_id=c.task_id
    ) merge ON true
    LEFT JOIN LATERAL (
        -- A single verified PR can satisfy this definition. The host does not
        -- establish a required set for multiple PRs, even if all were merged.
        SELECT count(DISTINCT pull_request) AS required_candidates,min(verified_at_ms) AS verified_at_ms
        FROM (
            SELECT pull_request, verified_at_ms
            FROM task_merge_clocks m
            WHERE m.project_id=c.project_id AND m.task_id=c.task_id
              AND m.repository_id=c.definition->>'repository_id'
              AND m.target_ref=c.definition->>'target_ref'
            UNION ALL
            SELECT (o.request->>'pull_request')::bigint,NULL
            FROM playbook_host_operations o
            WHERE o.command_name='pull_request.merge' AND o.request->>'project'=c.project_id::text
              AND o.request->>'task'=c.task_id::text AND o.created_at_ms<=$3
              AND o.request#>>'{merge_target,git,repository_id}'=c.definition->>'repository_id'
              AND 'refs/heads/'||(o.request#>>'{merge_target,default_branch}')=c.definition->>'target_ref'
              AND (o.request->>'pull_request') ~ '^[1-9][0-9]{0,9}$'
        ) observed
    ) milestones ON c.definition->>'kind'='target_branch_merge'
    LEFT JOIN LATERAL (
        SELECT report_id,submitted_at_ms FROM result_revision_pins p
        WHERE p.project_id=c.project_id AND p.task_id=c.task_id AND p.submitted_at_ms<=$3
        ORDER BY submitted_at_ms,report_id LIMIT 1
    ) first_report ON true
    LEFT JOIN LATERAL (
        SELECT CASE WHEN count(DISTINCT outcome)=1 THEN min(outcome) END AS outcome,at_ms
        FROM (
            SELECT CASE WHEN disposition='confirmed' THEN 'accepted' ELSE 'returned' END AS outcome,
                   updated_at_ms AS at_ms
            FROM profiled_auto_settlements
            WHERE report_id=first_report.report_id AND disposition IN ('confirmed','rework_required')
            UNION ALL
            SELECT disposition,created_at_ms FROM profiled_settlement_evaluations
            WHERE report_id=first_report.report_id AND disposition IN ('accepted','rejected')
            UNION ALL
            SELECT decision,decided_at_ms FROM delivery_decisions
            WHERE result_id=first_report.report_id AND task_id=c.task_id AND decision='returned'
        ) adjudications
        WHERE at_ms<=$3
        GROUP BY at_ms
        ORDER BY at_ms LIMIT 1
    ) decision ON true
    LEFT JOIN LATERAL (
        SELECT outcome,at_ms FROM (
            SELECT 'completed' AS outcome,completion.at_ms
            UNION ALL
            SELECT d.decision,d.decided_at_ms
            FROM result_revision_pins p JOIN delivery_decisions d
                ON d.result_id=p.report_id AND d.task_id=p.task_id
            WHERE p.project_id=c.project_id AND p.task_id=c.task_id AND d.decision<>'completed'
            UNION ALL
            SELECT CASE s.disposition WHEN 'held' THEN 'held'
                   WHEN 'rework_required' THEN 'returned' ELSE 'pending' END, s.updated_at_ms
            FROM result_revision_pins p JOIN profiled_auto_settlements s USING (report_id)
            WHERE p.project_id=c.project_id AND p.task_id=c.task_id AND s.disposition<>'confirmed'
            UNION ALL
            SELECT 'cancelled', x.recorded_at_ms
            FROM profiled_cancelled_settlements x JOIN profiled_launches l USING (attempt_id)
            JOIN execution_attempts a USING (attempt_id)
            WHERE a.task_id=c.task_id AND (l.execution_spec#>>'{lifecycle,project_id}')::uuid=c.project_id
        ) dispositions
        WHERE at_ms<=$3
        ORDER BY at_ms DESC,outcome LIMIT 1
    ) last_decision ON true
), configured AS (
    SELECT DISTINCT definition->>'kind' AS kind
    FROM (
        SELECT DISTINCT ON (project_id, work_class) definition
        FROM performance_delivery_definitions
        WHERE effective_at_ms <= $3 AND ($1 IS NULL OR project_id=$1)
        ORDER BY project_id,work_class,revision DESC
    ) active
    WHERE definition->>'kind' IN ('confirmed_completion','target_branch_merge')
    UNION
    SELECT definition->>'kind' FROM tasks
    WHERE definition->>'kind' IN ('confirmed_completion','target_branch_merge')
), timings AS (
    SELECT 'completion' AS kind, completion_at_ms-ready_at_ms AS elapsed
    FROM tasks WHERE completion_at_ms>$2
    UNION ALL
    SELECT 'merge', elapsed FROM merge_events
    UNION ALL
    SELECT 'delivery', delivery_at_ms-ready_at_ms FROM tasks WHERE delivery_at_ms>$2
), by_clock AS (
    SELECT kind,count(*) AS total,count(*) FILTER (WHERE elapsed>=0) AS covered,
        percentile_cont(0.5) WITHIN GROUP (ORDER BY elapsed) FILTER (WHERE elapsed>=0) AS median_ms,
        percentile_cont(0.9) WITHIN GROUP (ORDER BY elapsed) FILTER (WHERE elapsed>=0) AS p90_ms
    FROM timings GROUP BY kind
), timing AS (
    SELECT jsonb_object_agg(kind, jsonb_build_object('total',coalesce(total,0),
        'covered',coalesce(covered,0),'median_ms',median_ms,'p90_ms',p90_ms)) AS clocks
    FROM (VALUES ('completion'),('merge'),('delivery')) clock(kind) LEFT JOIN by_clock USING (kind)
), acceptance AS (
    SELECT count(*) FILTER (WHERE adjudicated_at_ms>$2 AND first_pass='accepted') AS numerator,
        count(*) FILTER (WHERE adjudicated_at_ms>$2) AS denominator,
        count(*) FILTER (WHERE adjudicated_at_ms IS NULL AND submitted_at_ms IS NOT NULL
            AND (submitted_at_ms>$2 OR completion_at_ms>$2 OR disposition='returned')) AS unknown,
        count(*) FILTER (WHERE definition IS NULL AND (completion_at_ms>$2 OR merge_at_ms>$2)) AS unknown_definitions,
        count(*) FILTER (WHERE delivery_evidence='required_pr_set_unknown') AS unknown_required_pr_sets
    FROM tasks
), examples AS (
    SELECT project_id,task_id,ready_at_ms AS first_ready_at_ms,work_class,definition,definition_revision,
           nullif(definition_class,'') AS definition_work_class,completion_at_ms,merge_at_ms,delivery_at_ms,
           first_pass,disposition,delivery_evidence
    FROM tasks
    ORDER BY greatest(completion_at_ms,merge_at_ms,submitted_at_ms,adjudicated_at_ms) DESC NULLS LAST, project_id,task_id
    LIMIT 50
), unclocked_pilot AS (
    SELECT count(DISTINCT (l.execution_spec#>>'{lifecycle,project_id}',c.task_id)) AS total
    FROM profiled_task_completions c JOIN profiled_launches l USING (attempt_id)
    WHERE c.state='confirmed' AND NOT EXISTS (
        SELECT 1 FROM performance_pilot_completion_times t WHERE t.operation_id=c.operation_id)
      AND ($1 IS NULL OR (l.execution_spec#>>'{lifecycle,project_id}')::uuid=$1)
)
SELECT jsonb_build_object(
    'confirmed_completions', clocks#>'{completion,total}',
    'verified_merges',clocks#>'{merge,total}',
    'completion_timing',clocks->'completion',
    'merge_timing',clocks->'merge',
    'completions_with_unknown_date',unclocked_pilot.total,
    'first_pass_acceptance',jsonb_build_object('numerator',numerator,'denominator',denominator,
        'value',numerator::double precision/nullif(denominator,0),'unknown',unknown),
    'delivery',jsonb_build_object(
        'state',CASE WHEN NOT EXISTS(SELECT FROM configured) THEN 'not_tracked'
                     WHEN unknown_definitions>0 OR unknown_required_pr_sets>0 THEN 'partial' ELSE 'available' END,
        'definitions',COALESCE((SELECT jsonb_agg(kind ORDER BY kind) FROM configured),'[]'),
        'delivered_tasks', CASE WHEN EXISTS(SELECT FROM configured) THEN clocks#>'{delivery,total}' END,
        'timing',clocks->'delivery',
        'unknown_definitions',unknown_definitions,
        'unknown_required_pr_sets',unknown_required_pr_sets),
    'tasks',COALESCE((SELECT jsonb_agg(to_jsonb(examples)) FROM examples),'[]')
) FROM timing,acceptance,unclocked_pilot
