//! Code-owned recipe registry schema 1.

mod auth;
mod families;
mod setup;

pub use auth::{
    AuthCredential, AuthMethodRecipe, AuthValidationError, CredentialKind, WireAuth, auth_method,
    auth_methods, validate_auth_definition, validate_auth_override,
};
pub use families::{
    COMPILER_VERSION, FamilyKind, FamilyRecipe, FamilyRecipeRegistry, FamilyResolutionError,
    ResolvedFamilyModel, ResolvedShape, compatible_auth_method, compatible_credential_field,
    environment_aliases, family_registry, placeholders, resolve_model, setup_field_name,
    substitute_placeholders,
};
pub use setup::{
    EndpointPolicy, SetupFieldRecipe, SetupFieldType, SetupRecipe, SetupValidationError,
    ValidatedSetup, validate_setup,
};

/// Frozen recipe-registry schema version.
pub const RECIPE_REGISTRY_SCHEMA_VERSION: u32 = 1;
