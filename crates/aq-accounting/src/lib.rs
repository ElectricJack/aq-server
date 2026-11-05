//! Pure proposals only. The caller verifies source/account/lineage policy and
//! applies event/call/counter/projection changes in one durable transaction.
pub use aq_lifecycle_protocol as wire;
use serde::{Deserialize, Serialize};
pub mod pricing;
pub mod projection;
pub mod reconcile;
pub mod vm_time;
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid accounting evidence")]
    Invalid,
    #[error("accounting arithmetic overflow")]
    Overflow,
    #[error("accounting evidence conflict")]
    Conflict,
    #[error("accounting policy is unsupported")]
    Unsupported,
}
pub type Result<T> = std::result::Result<T, Error>;
impl From<wire::ContractError> for Error {
    fn from(_: wire::ContractError) -> Self {
        Self::Invalid
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Input,
    Output,
    CacheRead,
    CacheWrite,
    Reasoning,
}
impl Category {
    pub const ALL: [Self; 5] = [
        Self::Input,
        Self::Output,
        Self::CacheRead,
        Self::CacheWrite,
        Self::Reasoning,
    ];
    fn bit(self) -> u8 {
        1 << self as u8
    }
    pub fn get(self, c: &wire::Counters) -> &wire::Counter {
        match self {
            Self::Input => &c.input,
            Self::Output => &c.output,
            Self::CacheRead => &c.cache_read,
            Self::CacheWrite => &c.cache_write,
            Self::Reasoning => &c.reasoning,
        }
    }
}
/// Relations are supplied by a verified category policy, not inferred from the
/// incoming provider's `DeclaredDisjoint` / `MayOverlap` claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    pub policy: wire::ArtifactRef,
    pub input_includes_cache_read: bool,
    pub input_includes_cache_write: bool,
    pub output_includes_reasoning: bool,
}
impl Layout {
    pub fn validate(&self) -> Result<()> {
        wire::artifact(&self.policy)?;
        Ok(())
    }
    pub fn atoms(&self, c: Category) -> u8 {
        c.bit()
            | match c {
                Category::Input => {
                    (if self.input_includes_cache_read {
                        Category::CacheRead.bit()
                    } else {
                        0
                    }) | (if self.input_includes_cache_write {
                        Category::CacheWrite.bit()
                    } else {
                        0
                    })
                }
                Category::Output if self.output_includes_reasoning => Category::Reasoning.bit(),
                _ => 0,
            }
    }
    pub fn check_counters(&self, c: &wire::Counters) -> Result<()> {
        self.validate()?;
        let subsets = |parent: Category, children: &[Category]| -> Result<()> {
            let mut sum = 0u64;
            for child in children {
                if let wire::Counter::Known { value } = child.get(c) {
                    sum = sum.checked_add(*value).ok_or(Error::Overflow)?;
                }
            }
            if let wire::Counter::Known { value } = parent.get(c)
                && sum > *value
            {
                return Err(Error::Conflict);
            }
            Ok(())
        };
        let mut caches = Vec::new();
        if self.input_includes_cache_read {
            caches.push(Category::CacheRead);
        }
        if self.input_includes_cache_write {
            caches.push(Category::CacheWrite);
        }
        subsets(Category::Input, &caches)?;
        subsets(
            Category::Output,
            if self.output_includes_reasoning {
                &[Category::Reasoning]
            } else {
                &[]
            },
        )
    }
    pub fn unique_total(&self, c: &wire::Counters) -> Result<wire::Counter> {
        self.check_counters(c)?;
        let mut total = 0u64;
        let mut unknown = None;
        for category in Category::ALL {
            if (category == Category::CacheRead && self.input_includes_cache_read)
                || (category == Category::CacheWrite && self.input_includes_cache_write)
                || (category == Category::Reasoning && self.output_includes_reasoning)
            {
                continue;
            }
            match category.get(c) {
                wire::Counter::Known { value } => {
                    total = total.checked_add(*value).ok_or(Error::Overflow)?
                }
                wire::Counter::Unknown { reason } => {
                    unknown.get_or_insert(reason.clone());
                }
            }
        }
        Ok(match unknown {
            Some(reason) => wire::Counter::Unknown { reason },
            None => wire::Counter::Known { value: total },
        })
    }
}
/// Subtraction is category-wise, never coercing unknown counters into zero.
pub fn subtract(current: &wire::Counters, baseline: &wire::Counters) -> Result<wire::Counters> {
    fn counter(a: &wire::Counter, b: &wire::Counter) -> Result<wire::Counter> {
        Ok(match (a, b) {
            (wire::Counter::Known { value: a }, wire::Counter::Known { value: b }) => {
                wire::Counter::Known {
                    value: a.checked_sub(*b).ok_or(Error::Conflict)?,
                }
            }
            (wire::Counter::Unknown { reason }, _) | (_, wire::Counter::Unknown { reason }) => {
                wire::Counter::Unknown {
                    reason: reason.clone(),
                }
            }
        })
    }
    Ok(wire::Counters {
        input: counter(&current.input, &baseline.input)?,
        output: counter(&current.output, &baseline.output)?,
        cache_read: counter(&current.cache_read, &baseline.cache_read)?,
        cache_write: counter(&current.cache_write, &baseline.cache_write)?,
        reasoning: counter(&current.reasoning, &baseline.reasoning)?,
    })
}
pub fn zeros() -> wire::Counters {
    let z = wire::Counter::Known { value: 0 };
    wire::Counters {
        input: z.clone(),
        output: z.clone(),
        cache_read: z.clone(),
        cache_write: z.clone(),
        reasoning: z,
    }
}

fn hash<T: Serialize>(value: &T) -> Result<wire::Digest> {
    let mut value = serde_json::to_value(value).map_err(|_| Error::Invalid)?;
    value.sort_all_objects();
    Ok(wire::Digest::of(
        &serde_json::to_vec(&value).map_err(|_| Error::Invalid)?,
    ))
}
