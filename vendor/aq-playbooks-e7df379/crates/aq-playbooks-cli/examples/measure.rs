//! Reproducible synthetic microbenchmark, deliberately separate from correctness tests.
use aq_playbooks_cli::{pilots, simulation};
use aq_playbooks_model::{EvalContext, EvalLimits, Expr, eval};
use std::{hint::black_box, time::Instant};
fn percentile(samples: &mut [u128], p: usize) -> u128 {
    samples.sort_unstable();
    samples[(samples.len() - 1) * p / 100]
}
fn main() -> anyhow::Result<()> {
    let expression = Expr::Eq {
        left: Box::new(Expr::literal(42)),
        right: Box::new(Expr::literal(42)),
    };
    let context = EvalContext::new();
    let limits = EvalLimits::default();
    let mut eval_samples = Vec::new();
    for _ in 0..10_000 {
        let start = Instant::now();
        black_box(eval(&expression, &context, &limits)?);
        eval_samples.push(start.elapsed().as_nanos());
    }
    let scenario = pilots::scenario("coding")?;
    let mut samples = Vec::new();
    let mut transitions = 0;
    for _ in 0..20 {
        let start = Instant::now();
        let report = simulation::simulate(&scenario)?;
        anyhow::ensure!(report.complete, "pilot incomplete");
        transitions = report.transitions;
        samples.push(start.elapsed().as_micros());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"profile":if cfg!(debug_assertions){"debug"}else{"release"},"expression_samples":10000,"expression_p95_ns":percentile(&mut eval_samples,95),"coding_samples":20,"coding_p95_us":percentile(&mut samples,95),"coding_transitions":transitions,"limitations":"Includes full memory snapshot cloning/validation. No production AQ effects, PostgreSQL, network or parallel workload; not a release performance threshold."})
        )?
    );
    Ok(())
}
