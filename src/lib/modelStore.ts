import { listModels } from "./chat";

// Global in-memory cache for the Nebius model list. Populated once on app mount;
// Settings dialog reads from here instead of re-fetching on every open.
let cache: string[] | null = null;
let pending: Promise<string[]> | null = null;
let lastError: string | null = null;

export async function ensureModels(): Promise<string[]> {
  if (cache) return cache;
  if (!pending) {
    pending = listModels()
      .then((ids) => {
        cache = ids;
        lastError = null;
        return ids;
      })
      .catch((e) => {
        lastError = e?.message ?? String(e);
        cache = [];
        throw e;
      });
  }
  return pending;
}

export function getCachedModels(): { models: string[] | null; error: string | null } {
  return { models: cache, error: lastError };
}

export function invalidateModels() {
  cache = null;
  pending = null;
  lastError = null;
}
