# Development notes

- This is an independent API compatibility implementation for wger 2.7.
- Do not add migrations or change the shared wger database schema.
- Use synthetic data only in tests and examples. Never commit real user data,
  API captures, production settings, secrets, or tokens.
- Keep credentials in environment variables and local ignored `.env` files.
- Run unit tests and the disposable contract harness before proposing changes.
- The harness must never target a production database or media volume.
