//! Provisional intelligence class from the model name (spec 0007 decision 8).
//! Phase 3 replaces this with the router's class.
use aq_dashboard_contract::IntelligenceClass;

/// `mini` counts only as a whole token, so `gemini` is not a mini model.
fn has_mini_token(model: &str) -> bool {
    model.match_indices("mini").any(|(i, _)| {
        let before = model[..i].chars().next_back();
        let after = model[i + 4..].chars().next();
        before.is_none_or(|c| c == '-') && after.is_none_or(|c| c == '-' || c.is_ascii_digit())
    })
}

pub fn intelligence_class(model: &str) -> IntelligenceClass {
    let model = model.to_lowercase();
    let has = |names: &[&str]| names.iter().any(|n| model.contains(n));
    if has(&["flash", "haiku", "spark"]) || has_mini_token(&model) {
        IntelligenceClass::Fast
    } else if has(&["opus"]) {
        IntelligenceClass::Deep
    } else {
        IntelligenceClass::Standard
    }
}
