//! Code-owned recipe registry schema 1.

mod auth;
mod claims;
mod providers;
mod setup;

pub use auth::{
    AuthCredential, AuthMethodRecipe, AuthValidationError, CredentialKind, WireAuth, auth_method,
    auth_methods, validate_auth_definition, validate_auth_override,
};
pub use claims::{
    CatalogClaim, CatalogModelClaimInput, CatalogProviderClaimInput, ClaimPresence,
    ModelRecipeMatch, ProviderRecipeMatch, RecipeQuarantineReason,
};
pub use providers::{
    COMPILER_VERSION, ProviderRecipe, RecipeRegistry, registry1, route_openai_model,
};
pub use setup::{
    EndpointPolicy, SetupFieldRecipe, SetupFieldType, SetupRecipe, SetupValidationError,
    ValidatedSetup, validate_setup,
};

/// Frozen recipe-registry schema version.
pub const RECIPE_REGISTRY_SCHEMA_VERSION: u32 = 1;
