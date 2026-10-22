/**
 * Records what one extension load sees of module state left by earlier
 * loads in the same process. Appends one JSON line per load to
 * PI_FAMULUS_TEST_PROBE_LOG.
 */
import { appendFileSync } from "node:fs";
import { randomUUID } from "node:crypto";

const shared = { value: "init", owners: [] };

export default function isolationProbe() {
  const log = process.env.PI_FAMULUS_TEST_PROBE_LOG;
  const id = randomUUID();
  const seen = shared.value;
  shared.value = id;
  shared.owners.push(id);
  if (log) {
    appendFileSync(log, JSON.stringify({
      id,
      pid: process.pid,
      childId: process.env.PI_FAMULUS_CHILD_ID ?? null,
      seen,
      owners: shared.owners.slice(),
    }) + "\n");
  }
}
