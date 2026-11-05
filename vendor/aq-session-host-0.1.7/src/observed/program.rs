//! Private typed dispatch; legacy sealed owners retain their original policy.
use super::*;
pub(crate) enum ObservedProgram {
    Sealed(QualifiedElf),
    Named(NamedReadonlyExecImage),
}
impl ObservedProgram {
    pub(crate) fn identity(&self) -> &ElfIdentity {
        match self {
            Self::Sealed(p) => p.identity(),
            Self::Named(p) => p.0.identity(),
        }
    }
    pub(crate) fn source(&self) -> &Path {
        match self {
            Self::Sealed(p) => p.source(),
            Self::Named(p) => p.0.source(),
        }
    }
    pub(crate) fn file(&self) -> &File {
        match self {
            Self::Sealed(p) => p.file(),
            Self::Named(p) => p.0.file(),
        }
    }
}
