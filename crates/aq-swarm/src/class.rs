//! Provisional intelligence class from the model name (spec 0007 decision 8).
//! Phase 3 replaces this with the router's class.
use aq_dashboard_contract::IntelligenceClass;

pub fn intelligence_class(model: &str) -> Option<IntelligenceClass> {
    let model = model.to_lowercase();
    let has = |names: &[&str]| names.iter().any(|n| model.contains(n));
    if has(&["haiku", "mini", "spark", "flash"]) {
        Some(IntelligenceClass::Fast)
    } else if has(&["sonnet"]) {
        Some(IntelligenceClass::Standard)
    } else if has(&["opus"]) {
        Some(IntelligenceClass::Deep)
    } else {
        None
    }
}
