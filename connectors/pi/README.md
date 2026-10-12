# pi

The exception among the connectors (ADR-0217): not a model vendor but a credential source. No
route and no usage probe; only the module (`components/`):

- `connectors/pi`: the credential method that borrows the pi CLI's login (the `pi` format of
  `connectors/shared/logins`), for an account that names it.
