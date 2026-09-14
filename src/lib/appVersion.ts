import { getIdentifier } from "@tauri-apps/api/app";
import upstreamRelease from "../../upstream-release.json";
import { readAppVersion } from "./updater";

export function formatAppVersion(
  version: string,
  upstreamVersion?: string,
): string {
  return upstreamVersion ? `${version} (${upstreamVersion})` : version;
}

export async function readAppBuildInfo() {
  const [version, identifier] = await Promise.all([
    readAppVersion(),
    getIdentifier().catch(() => ""),
  ]);
  return {
    version,
    upstream: identifier.startsWith("com.monocode.fork.")
      ? upstreamRelease
      : undefined,
  };
}
