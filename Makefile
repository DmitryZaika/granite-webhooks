# Variables
REGION := us-east-2
AWS_DEPLOY_PROFILE ?= personal
IAM_ROLE := arn:aws:iam::741448943665:role/cargo-lambda-role-2ed5069c-8882-460d-bdc8-192d9b724756

# Tool commands
BUILD_BASE := uvx cargo-lambda lambda build --release --x86-64

# --- Build and deploy ---
#   make build <app>          app: api, webhooks, time-triggered
#   make deploy <app> <env>   env: dev, prod
#   make deploy-all           api and webhooks, each to dev and prod
# Each app is a crate whose binary has the same name.
APPS := api webhooks time-triggered
ENVS := dev prod
APP ?= $(firstword $(filter $(APPS),$(MAKECMDGOALS)))
ENV ?= $(firstword $(filter $(ENVS),$(MAKECMDGOALS)))

# The Lambdas of each app in each environment (time-triggered has no dev one).
# granite-api-stream is the same binary behind a streaming function URL, for
# answers that stream or outlast API Gateway's 30 s (LAMBDA_RESPONSE_STREAMING=1).
FUNCTION_api_prod := granite-api granite-api-stream
FUNCTION_api_dev := granite-api-dev
FUNCTION_webhooks_prod := granite-webhooks
FUNCTION_webhooks_dev := granite-webhooks-dev
FUNCTION_time-triggered_prod := time-triggered
FUNCTION = $(FUNCTION_$(APP)_$(ENV))

# The app and environment words are goals too; they do nothing themselves.
.PHONY: $(APPS) $(ENVS)
$(APPS) $(ENVS):
	@:

.PHONY: build
build:
	@test -n "$(APP)" || { echo "Usage: make build <$(APPS)>" >&2; exit 1; }
	GIT_SHA=$$(git rev-parse --short HEAD) $(BUILD_BASE) -p $(APP) --bin $(APP)

# Deploy exports credentials because cargo-lambda does not support AWS login_session profiles.
# It updates the code only: environment variables set on the Lambda are kept.
.PHONY: deploy
deploy:
	@test -n "$(APP)" -a -n "$(ENV)" || { echo "Usage: make deploy <$(APPS)> <$(ENVS)>" >&2; exit 1; }
	@test -n "$(FUNCTION)" || { echo "$(APP) has no $(ENV) Lambda" >&2; exit 1; }
	$(MAKE) build APP=$(APP)
	@eval "$$(aws configure export-credentials --profile $(AWS_DEPLOY_PROFILE) --format env)" && \
	unset AWS_PROFILE && \
	for function in $(FUNCTION); do \
		uvx cargo-lambda lambda deploy \
			--iam-role $(IAM_ROLE) \
			--region $(REGION) \
			--binary-name $(APP) \
			$$function || exit 1; \
	done

# api and webhooks, each built and deployed to dev and prod.
.PHONY: deploy-all
deploy-all:
	$(MAKE) deploy APP=api ENV=dev
	$(MAKE) deploy APP=api ENV=prod
	$(MAKE) deploy APP=webhooks ENV=dev
	$(MAKE) deploy APP=webhooks ENV=prod

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
	@eval "$$(aws configure export-credentials --profile $(AWS_DEPLOY_PROFILE) --format env)" && \
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

# --- API (frontend-facing Lambda, see api/README.md) ---
# Override LOCAL_DB_NAME to keep a second seeded database next to granite_local
# (e.g. E2E runs of another worktree): `make api-db-reset LOCAL_DB_NAME=x`.
LOCAL_DB_NAME ?= granite_local
LOCAL_DB_URL := mysql://root:granite@127.0.0.1:3307/$(LOCAL_DB_NAME)

.PHONY: api-db-up
api-db-up:
	docker compose -f docker-compose.local.yml up -d --wait

# Drop, re-migrate and re-seed the local database.
.PHONY: api-db-reset
api-db-reset: api-db-up
	docker exec granite-local-mysql mysql -uroot -pgranite -e "DROP DATABASE IF EXISTS $(LOCAL_DB_NAME); CREATE DATABASE $(LOCAL_DB_NAME);"
	DATABASE_URL=$(LOCAL_DB_URL) cargo sqlx migrate run --source migrations
	docker exec -i granite-local-mysql mysql --default-character-set=utf8mb4 -uroot -pgranite $(LOCAL_DB_NAME) < api/seed/local_seed.sql

.PHONY: api-test
api-test: api-db-up
	DATABASE_URL=$(LOCAL_DB_URL) cargo test -p api

# Regenerates api/openapi.json from the route annotations (no database needed).
.PHONY: api-openapi
api-openapi:
	UPDATE_OPENAPI=1 cargo test -p api --test openapi spec_file_is_current

# Serves http://localhost:9100/lambda-url/api/... through the Lambda runtime emulator.
.PHONY: api-local
api-local: api-db-up
	test -f api/.env.local || cp api/.env.local.example api/.env.local
	$(WATCH_BASE) -p api --bin api -P 9100 --env-file api/.env.local
