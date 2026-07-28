# Variables
REGION := us-east-2
IAM_ROLE := arn:aws:iam::741448943665:role/cargo-lambda-role-2ed5069c-8882-460d-bdc8-192d9b724756

# Tool commands
BUILD_BASE := uvx cargo-lambda lambda build --release --x86-64

# Deploy exports credentials because cargo-lambda does not support AWS login_session profiles.

# --- Webhooks ---
.PHONY: build-webhooks
build-webhooks:
	$(BUILD_BASE) -p webhooks --bin webhooks

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
	$(BUILD_BASE) -p time-triggered --bin time-triggered

.PHONY: deploy-time-triggered
deploy-time-triggered: build-time-triggered
	@eval "$$(aws configure export-credentials --format env)" && \
	unset AWS_PROFILE && \
	uvx cargo-lambda lambda deploy \
		--iam-role $(IAM_ROLE) \
		--region $(REGION) \
		--binary-name time-triggered \
		time-triggered
