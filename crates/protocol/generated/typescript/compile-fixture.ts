/// <reference path="./globals.d.ts" />
import type {
  ClientHello,
  ModelSnapshotManifestV1,
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
] | null = null;

export { roots };
