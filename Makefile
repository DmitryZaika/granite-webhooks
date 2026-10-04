# Variables
REGION := us-east-2
AWS_PROFILE_NAME := personal
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
	@creds="$$(aws configure export-credentials --profile $(AWS_PROFILE_NAME) --format env)" || exit 1; \
	eval "$$creds" && \
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
	@creds="$$(aws configure export-credentials --profile $(AWS_PROFILE_NAME) --format env)" || exit 1; \
	eval "$$creds" && \
	unset AWS_PROFILE && \
	uvx cargo-lambda lambda deploy \
		--iam-role $(IAM_ROLE) \
		--region $(REGION) \
		--binary-name time-triggered \
		time-triggered

# --- API (frontend-facing Lambda, see api/README.md) ---
LOCAL_DB_URL := mysql://root:granite@127.0.0.1:3307/granite_local

.PHONY: api-db-up
api-db-up:
	docker compose -f docker-compose.local.yml up -d --wait

# Drop, re-migrate and re-seed the local database.
.PHONY: api-db-reset
api-db-reset: api-db-up
	docker exec granite-local-mysql mysql -uroot -pgranite -e "DROP DATABASE IF EXISTS granite_local; CREATE DATABASE granite_local;"
	DATABASE_URL=$(LOCAL_DB_URL) cargo sqlx migrate run --source migrations
	docker exec -i granite-local-mysql mysql -uroot -pgranite granite_local < api/seed/local_seed.sql

.PHONY: api-test
api-test: api-db-up
	DATABASE_URL=$(LOCAL_DB_URL) cargo test -p api

# Serves http://localhost:9100/lambda-url/api/... through the Lambda runtime emulator.
.PHONY: api-local
api-local: api-db-up
	test -f api/.env.local || cp api/.env.local.example api/.env.local
	$(WATCH_BASE) -p api --bin api -P 9100 --env-file api/.env.local

.PHONY: build-api
build-api:
	$(BUILD_BASE) -p api --bin api

.PHONY: deploy-api
deploy-api: build-api
	@creds="$$(aws configure export-credentials --profile $(AWS_PROFILE_NAME) --format env)" || exit 1; \
	eval "$$creds" && \
	unset AWS_PROFILE && \
	uvx cargo-lambda lambda deploy \
		--iam-role $(IAM_ROLE) \
		--region $(REGION) \
		--binary-name api \
		granite-api
