/**
 * Small helpers the wave-2 bridge modules share on top of `backend.ts`:
 * subscribing to backend events and invoking commands that stream progress
 * over a Tauri `Channel`. Kept separate from `backend.ts` so that module
 * stays the single owner of `invoke`.
 */
import { Channel } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { call, inTauri } from "./backend";

/**
 * Subscribes to a backend event.
 *
 * @param event - Event name (e.g. `cleanup://queue-changed`).
 * @param onPayload - Receives each payload.
 * @returns Unsubscribe function (a no-op outside Tauri).
 */
// eslint-disable-next-line @typescript-eslint/no-unnecessary-type-parameters -- T declares the payload type, like invoke<T>.
export function listenEvent<T>(event: string, onPayload: (payload: T) => void): () => void {
  if (!inTauri()) return () => undefined;
  const un = listen<T>(event, (e) => {
    onPayload(e.payload);
  });
  return () => {
    void un.then((f) => {
      f();
    });
  };
}

/**
 * Invokes a command that streams events over a channel argument before it
 * resolves with its final result.
 *
 * @param command - Tauri command name.
 * @param channelArg - Name of the channel argument (e.g. `onProgress`).
 * @param args - Other arguments.
 * @param onEvent - Receives each streamed event, in order.
 * @returns The command's final result.
 */
// eslint-disable-next-line @typescript-eslint/no-unnecessary-type-parameters -- E declares the event type, like invoke<T>.
export function callWithChannel<T, E>(
  command: string,
  channelArg: string,
  args: Record<string, unknown>,
  onEvent: (event: E) => void,
): Promise<T> {
  // NOTE: constructing a Channel outside Tauri touches `window.__TAURI_INTERNALS__`;
  // `call` rejects with BackendUnavailableError there anyway, so skip it.
  if (!inTauri()) return call<T>(command, args);
  const channel = new Channel<E>();
  channel.onmessage = onEvent;
  return call<T>(command, { ...args, [channelArg]: channel });
}
