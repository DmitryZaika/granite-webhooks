# Variables
REGION := us-east-2
IAM_ROLE := arn:aws:iam::741448943665:role/cargo-lambda-role-2ed5069c-8882-460d-bdc8-192d9b724756

# Tool commands
BUILD_BASE := uvx cargo-lambda lambda build --release

# We use AWS_PROFILE=default at the start of the command to force the credential choice
DEPLOY_BASE := AWS_PROFILE=default uvx cargo-lambda lambda deploy --iam-role $(IAM_ROLE) --region $(REGION)

# --- Webhooks ---
.PHONY: build-webhooks
build-webhooks:
	$(BUILD_BASE) -p webhooks --bin webhooks

.PHONY: deploy-webhooks
deploy-webhooks: build-webhooks
	$(DEPLOY_BASE) --binary-name webhooks granite-webhooks

# --- Time-Triggered ---
.PHONY: build-time-triggered
build-time-triggered:
	$(BUILD_BASE) -p time-triggered --bin time-triggered

.PHONY: deploy-time-triggered
deploy-time-triggered: build-time-triggered
	$(DEPLOY_BASE) --binary-name time-triggered time-triggered
