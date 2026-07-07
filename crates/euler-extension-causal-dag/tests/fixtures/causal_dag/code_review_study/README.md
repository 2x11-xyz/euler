# Code-Review Study Fixture

Fresh current-Euler fixture for an agent/review workflow projection.

Acceptance shape:
- agent and tool events are source substrate, not graph vocabulary;
- findings are projected as claim nodes;
- `extension.artifact` validates review-report path, hash, byte length, and source coverage;
- refutation and supersedes edges remain annotations;
- verification is a structural backbone edge backed by a check result.
