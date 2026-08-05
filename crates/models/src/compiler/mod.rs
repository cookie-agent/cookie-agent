//! Registry-1 dynamic managed/custom model compiler.

mod dynamic;
mod executable;
mod fingerprint;
mod projection;
mod variants;

pub use dynamic::{
    AuthSourceCategory, CompiledAuthShape, CompiledDynamicModel, CompiledDynamicProvider,
    CompiledModelStatus, DynamicCompileError, DynamicCompiler, ModelQuarantine,
};
pub use variants::{CompiledVariant, CompiledVariantOrigin};

pub(crate) use executable::{
    ExecutableBehaviorInput, ExecutableCredentialMaterial, compile_executable,
};
