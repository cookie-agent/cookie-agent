declare global {
  type AgentId = string;
  type ProviderId = string;
  type ProviderModelId = string;
  type ModelKey = string;
  type VariantId = string;
  type SetupFieldId = string;
  type AuthFieldName = string;
  type AuthMethodId = string;
  type ProviderRecipeId = string;
  type ProtocolRecipeId = string;
  type ProviderSetupRecipeId = string;
  type RecipeCompilerVersion = string;
  type CatalogRevision = string;
  type RecipeRegistryRevision = string;
  type ProviderStoreRevision = string;
  type ProviderStateRevision = string;
  type ModelRevision = string;
  type AgentRevision = string;
  type RuntimeRevision = string;
  type ModelSnapshotRevision = string;
  type ModelSelection = { model: ModelKey; variant: VariantId | null };
  type LanguageModelDescriptor = {
    identity: { provider_id: string; model_id: string };
    adapter_id: string;
    capabilities: {
      features: Array<"tool_calling" | "parallel_tools" | "tool_input_deltas" | "reasoning" | "structured_output" | "temperature" | "top_p" | "max_output_tokens" | "prompt_caching" | "usage" | "provider_tools" | "sources">;
      limits: { context: number | null; input: number | null; output: number | null };
      modalities: { input: Array<string>; output: Array<string> };
      media: { input: Record<string, { media_types: Array<string>; sources: Array<"inline_bytes" | "inline_text" | "url" | "provider_reference"> }> };
      cancellation: "local_only" | "remote_best_effort" | "unsupported";
      compaction: "unsupported";
      replay: { policy: "never" | "if_valid" | "always"; capability: "required" | "optional" | "unsupported"; reasoning: boolean };
    };
    provider_metadata: Record<string, unknown>;
  };
}
export {};
