/// <reference path="./globals.d.ts" />
import type {
  AgentListResult,
  CatalogModelListResult,
  CatalogProviderListResult,
  ClientHello,
  ModelListResult,
  ProviderConnectParams,
  Request,
  RunStartParams,
  SessionCreateParams,
  StoredEvent,
} from "./index.js";

const roots: [
  ClientHello,
  Request,
  SessionCreateParams,
  RunStartParams,
  StoredEvent,
  CatalogProviderListResult,
  CatalogModelListResult,
  ProviderConnectParams,
  ModelListResult,
  AgentListResult,
] | null = null;

export { roots };
