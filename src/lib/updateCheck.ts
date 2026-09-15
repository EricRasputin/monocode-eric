import { invoke } from "@tauri-apps/api/core";
import { Update } from "@tauri-apps/plugin-updater";

type UpdateMetadata = ConstructorParameters<typeof Update>[0];

export async function checkForUpdate(): Promise<Update | null> {
  // The native command bounds both connection establishment and feed retrieval.
  // Reuse Tauri's Update resource for signature verification and installation.
  const metadata = await invoke<UpdateMetadata | null>("check_for_update");
  return metadata ? new Update(metadata) : null;
}
