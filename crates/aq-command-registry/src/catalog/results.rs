//! Published results, review evidence and task feedback.
use super::*;

pub(super) const COMMANDS: &[CommandSpec] = &[
    owner(
        "result.list",
        op::RESULT_READ,
        "Results",
        "/api/human/results",
    ),
    read(
        "result.show",
        op::RESULT_READ,
        TargetRule::Own,
        "One result revision",
    )
    .with_owner(Some(Route::get(
        "/api/human/results/{id}/revisions/{revision}",
        &[("id", "result_id")],
    ))),
    read(
        "result.media.list",
        op::RESULT_READ,
        TargetRule::Own,
        "Media attached to a result revision",
    )
    .with_owner(Some(Route::get(
        "/api/human/results/{report}/revisions/{revision}/media",
        &[("report", "result_id")],
    ))),
    owner(
        "result.feed",
        op::RESULT_READ,
        "The results feed",
        "/api/v1/results",
    ),
    read(
        "result.detail",
        op::RESULT_READ,
        TargetRule::Own,
        "One result in the feed",
    )
    .with_owner(Some(Route::get(
        "/api/v1/results/{id}/revisions/{revision}",
        &[("id", "result_id")],
    ))),
    read(
        "result.comments",
        op::RESULT_READ,
        TargetRule::Own,
        "A result's comment threads",
    )
    .with_owner(Some(Route::get(
        "/api/v1/results/{id}/revisions/{revision}/comments",
        &[("id", "result_id")],
    ))),
    // A reviewing agent's project-scoped read of retained results: it needs
    // `result.read@project:self` (not the owner's session) and answers from
    // the same validated owner reads as `result.list` and `result.show`,
    // beside where the retained work (the exact diff) is kept.
    agent(
        read(
            "result.review.list",
            op::RESULT_READ,
            P,
            "Retained results of the attempt's project; a report that cannot be read is listed as unavailable and the page says it is incomplete",
        ),
        Route::post(
            "/v1/agent/results/list",
            Body::Versioned,
            Params::Inline(
                r#"{"type":"object","additionalProperties":false,"properties":{"project_id":{"type":"string","format":"uuid","description":"Omitted: the attempt's project; another is refused"},"after":{"type":"string","format":"uuid","description":"The last report of the previous page"},"limit":{"type":"integer","minimum":1,"maximum":25}}}"#,
            ),
        ),
    ),
    agent(
        read(
            "result.review.show",
            op::RESULT_READ,
            P,
            "One retained result revision of the attempt's project, with where its work is retained (ref, base, head, provenance digest) and how to read the exact diff",
        ),
        Route::post(
            "/v1/agent/results/show",
            Body::Versioned,
            Params::Inline(
                r#"{"type":"object","additionalProperties":false,"required":["report_id","revision"],"properties":{"project_id":{"type":"string","format":"uuid","description":"Omitted: the attempt's project; another is refused"},"report_id":{"type":"string","format":"uuid"},"revision":{"type":"string","pattern":"^[0-9a-f]{64}$","description":"The exact report digest"}}}"#,
            ),
        ),
    ),
    agent(
        read(
            "result.review.patch",
            op::RESULT_READ,
            P,
            "The exact retained patch of one result revision of the attempt's project (base to head as recorded by AQ), with size and sha256 verified against aq-git's diff.patch answer; reports why no bytes are served when the work is missing, too large or aq-git lacks diff.patch",
        ),
        Route::post(
            "/v1/agent/results/patch",
            Body::Versioned,
            Params::Inline(
                r#"{"type":"object","additionalProperties":false,"required":["report_id","revision"],"properties":{"project_id":{"type":"string","format":"uuid","description":"Omitted: the attempt's project; another is refused"},"report_id":{"type":"string","format":"uuid"},"revision":{"type":"string","pattern":"^[0-9a-f]{64}$","description":"The exact report digest"},"max_bytes":{"type":"integer","minimum":1,"maximum":4194304,"description":"The most raw patch bytes to accept; default and maximum 4194304"}}}"#,
            ),
        ),
    ),
    read(
        "feedback.task.show",
        op::RESULT_READ,
        TargetRule::Own,
        "A feedback task draft",
    )
    .with_owner(Some(Route::get(
        "/api/v1/feedback-tasks/{id}",
        &[("id", "draft_id")],
    ))),
];
