# Variables
REGION := us-east-2
IAM_ROLE := arn:aws:iam::741448943665:role/cargo-lambda-role-2ed5069c-8882-460d-bdc8-192d9b724756

# Tool commands
BUILD_BASE := uvx cargo-lambda lambda build --release --x86-64

# Deploy exports credentials because cargo-lambda does not support AWS login_session profiles.

# --- Webhooks ---
.PHONY: build-webhooks
build-webhooks:
	GIT_SHA=$$(git rev-parse --short HEAD) $(BUILD_BASE) -p webhooks --bin webhooks

.PHONY: deploy-webhooks
deploy-webhooks: build-webhooks
	@eval "$$(aws configure export-credentials --format env)" && \
	unset AWS_PROFILE && \
	uvx cargo-lambda lambda deploy \
		--iam-role $(IAM_ROLE) \
		--region $(REGION) \
		--binary-name webhooks \
		granite-webhooks

# --- Local ---
WATCH_BASE := uvx cargo-lambda lambda watch --release

.PHONY: local-webhooks
local-webhooks:
	$(WATCH_BASE) -p webhooks --bin webhooks

.PHONY: local-time-triggered
local-time-triggered:
	$(WATCH_BASE) -p time-triggered --bin time-triggered

# --- Time-Triggered ---
.PHONY: build-time-triggered
build-time-triggered:
	GIT_SHA=$$(git rev-parse --short HEAD) $(BUILD_BASE) -p time-triggered --bin time-triggered

.PHONY: deploy-time-triggered
deploy-time-triggered: build-time-triggered
	@eval "$$(aws configure export-credentials --format env)" && \
	unset AWS_PROFILE && \
	uvx cargo-lambda lambda deploy \
		--iam-role $(IAM_ROLE) \
		--region $(REGION) \
		--binary-name time-triggered \
		time-triggered

# --- Tests ---
# Refuses to run cargo test against the shared RDS. DATABASE_URL is read from
# the environment, falling back to .env, and is never printed.
.PHONY: test
test:
	@URL="$${DATABASE_URL:-}"; \
	if [ -z "$$URL" ] && [ -f .env ]; then \
		URL=$$(grep -E '^DATABASE_URL=' .env | tail -n1 | cut -d'=' -f2-); \
	fi; \
	if printf '%s' "$$URL" | grep -Eiq '\.rds\.amazonaws\.com'; then \
		echo "Refusing: DATABASE_URL points at an RDS host. Tests must run against a local MySQL, never the shared RDS."; \
		exit 1; \
	fi; \
	cargo test
