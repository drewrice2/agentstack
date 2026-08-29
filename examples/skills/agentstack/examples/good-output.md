# Good output example

Scenario: the user asked to install the approved engineering stack into the
Codex target.

User:

```text
Install our engineering stack for Codex: acme/engineering-default.
```

Agent response:

```text
I installed approved stack `acme/engineering-default` into target `codex`.

1. What ran
   - `agentstack stack install acme/engineering-default --target codex`
   - `agentstack stack show acme/engineering-default --target codex`
   - `agentstack stack update acme/engineering-default --target codex --check`

2. What changed
   - AgentStack wrote a managed stack receipt for `engineering-default`.
   - The stack installed 3 approved child skills into the `codex` target.
   - No candidate versions were created or approved.

3. What was verified
   - `stack install` exited 0.
   - `agentstack stack show acme/engineering-default --target codex` found the stack
     receipt and child provenance.
   - `install list --kind stack --target codex --json` includes
     `engineering-default`.

4. What is pending
   - Nothing is pending for this install.
   - Future stack changes require an explicit update check and apply step.

5. Next command
   - `agentstack stack update acme/engineering-default --target codex --check`
```
