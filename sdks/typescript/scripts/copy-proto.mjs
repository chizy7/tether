// The package ships its own copy of the service definition, which the client
// loads when it starts. The one true copy is proto/tether.proto at the root
// of the repository; this brings it here before every build.
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
mkdirSync(join(here, "..", "proto"), { recursive: true });
copyFileSync(
  join(here, "..", "..", "..", "proto", "tether.proto"),
  join(here, "..", "proto", "tether.proto"),
);
