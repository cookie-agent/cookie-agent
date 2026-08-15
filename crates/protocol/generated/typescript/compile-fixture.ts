/// <reference path="./globals.d.ts" />
import type {
  ClientHello,
  ModelSnapshotManifestV1,
  McpApprovalListResult,
  McpApprovalRespondParams,
  ProviderConnectResult,
  ProviderDisconnectParams,
  Request,
  RuntimeChangedNotification,
  RuntimeSnapshotResult,
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
  RuntimeSnapshotResult,
  RuntimeChangedNotification,
  ProviderConnectResult,
  ProviderDisconnectParams,
  ModelSnapshotManifestV1,
  McpApprovalListResult,
  McpApprovalRespondParams,
] | null = null;

export { roots };
