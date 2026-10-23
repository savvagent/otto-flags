# Integrate end to end

Follow these steps in order; the language guide starts after them.

1. **Check access.** Call `whoami`: it shows the organization and your scopes. Creating flags and
   turning them on needs `flags:write`. Creating an app or rotating its keys needs `apps:admin` and
   an owner or admin role.
2. **Pick or create the app.** Call `list_apps` and choose the app for this project, or call
   `create_app` if it has none (its environments default to `production` only; pass the list the
   project needs). Pass the app's name to `sdk_guide` to fill this text in. It is filled in for
   app `{{APP_NAME}}` (id `{{APP_ID}}`), whose environments are {{ENVIRONMENTS}}.
{{KEY_STEP}}
4. **Create the flag first.** `create_flag` makes it switched off in every environment. Do it
   before any code references the flag.
5. **Wire the SDK.** Follow the guide below: install, initialize, evaluate. The `environment` the
   SDK evaluates in must be one of the app's environments ({{ENVIRONMENTS}}); read it from
   configuration instead of hardcoding it.
6. **Verify the integration before turning anything on.**
   - `evaluate_flag` shows what the server would serve a given user.
   - Run the app, or a one-off script with the same key and environment, and evaluate the flag.
     The SDKs never throw on a lookup: a failure returns the configured default, so a value that
     disagrees with `evaluate_flag`, or a result whose reason is `error`, means the app is not
     reaching the server with a valid key and environment. Check its logs.
   - After real traffic, `flag_health` shows the evaluations the SDKs reported. It can be empty at
     first; that alone is not a failure.
7. **Turn it on.** `set_flag_environment` with the `expected_version` from `get_flag` and a
   `reason`. Raise a rollout in steps and call `flag_health` between them.
8. **Know what is billed.** Each hour of an app is billed at the larger of the evaluations the
   server answered and the evaluations SDKs reported with the server key. With telemetry on (the
   default), a server-key SDK reports every evaluation, including ones answered from its cache or
   locally, so a flag on a hot path counts even when cached; with telemetry off only the
   server-answered ones count. Reports made with the public client key are never billed. Every MCP
   tool, `evaluate_flag` included, is free. The free allowance is set on the platform, not here:
   call `usage` for this month's meters, the budget and any block. A blocked organization's
   evaluations keep serving.
