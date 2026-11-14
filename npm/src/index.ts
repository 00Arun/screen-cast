/**
 * screen-cast: secure screen casting for web apps.
 *
 * Browser:  import { CastHost, CastViewer } from 'screen-cast';
 * Elements: import 'screen-cast/elements';   // <screen-cast-host>, <screen-cast-viewer>
 * Server:   import { createCastServer } from 'screen-cast/server';
 */
export { CastHost } from './host.js';
export type { CastHostOptions, CastHostEvents, CastSession, CastSource, StartOptions } from './host.js';
export { CastViewer } from './viewer.js';
export type { CastViewerOptions, CastViewerEvents, JoinOptions, ViewerStatus } from './viewer.js';
export { CastError } from './client.js';
export type { Signal } from './protocol.js';
