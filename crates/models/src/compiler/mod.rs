//! Registry-1 dynamic managed/custom model compiler.

mod dynamic;
mod executable;
mod fingerprint;
mod projection;
mod variants;

pub use dynamic::{
    AuthSourceCategory, CompiledAuthShape, CompiledDynamicModel, CompiledDynamicProvider,
    CompiledModelStatus, DynamicCompileError, DynamicCompiler, UnsupportedModel,
};
pub use variants::{CompiledVariant, CompiledVariantOrigin};

pub(crate) use dynamic::{managed_provider_adapter, validate_managed_cache};
pub(crate) use executable::{
    ExecutableBehaviorInput, ExecutableCredentialMaterial, compile_executable,
};
