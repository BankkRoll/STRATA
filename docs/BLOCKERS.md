# Blockers

Things that cannot be completed in the current environment, with the exact reason.

- **CI not run.** The repo has no GitHub remote yet. M0's acceptance (CI green on x64 + ARM64)
  needs the owner to push to GitHub.
- **Dev session not elevated.** Raw volume reads (`\.\C:`), USN journal access, VHDX mounting
  and ETW need an elevated session. Parser work is tested on synthetic in-memory NTFS images; real
  volume runs and VHDX fixture tests need an elevated run by the owner.
- **Code signing.** No signing certificate / Azure Trusted Signing account configured (M14).
