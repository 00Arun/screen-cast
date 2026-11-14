/**
 * screen-cast: secure screen casting for web apps.
 *
 * Browser:  import { CastHost, CastViewer } from 'secure-screen-cast';
 * Elements: import 'secure-screen-cast/elements';   // <screen-cast-host>, <screen-cast-viewer>
 * Server:   import { createCastServer } from 'secure-screen-cast/server';
 */
export { CastHost } from './host.js';
export type { CastHostOptions, CastHostEvents, CastSession, CastSource, StartOptions } from './host.js';
export { CastViewer } from './viewer.js';
export type { CastViewerOptions, CastViewerEvents, JoinOptions, ViewerStatus } from './viewer.js';
export { CastError } from './client.js';
export type { Signal } from './protocol.js';
