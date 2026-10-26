# Variables
REGION := us-east-2
AWS_DEPLOY_PROFILE ?= personal
IAM_ROLE := arn:aws:iam::741448943665:role/cargo-lambda-role-2ed5069c-8882-460d-bdc8-192d9b724756

# Tool commands
# cargo-lambda installed on the PATH when there is one (`cargo install
# cargo-lambda`: Windows on ARM has no wheel for uvx), else through uvx.
CARGO_LAMBDA ?= $(if $(shell command -v cargo-lambda 2>/dev/null),cargo-lambda,uvx cargo-lambda)
# Dev (make build/deploy <app> dev) is a debug build, stripped to fit in a
# Lambda, as in deploy-lambda.yml; the two variables only change the dev profile.
BUILD_BASE = CARGO_PROFILE_DEV_DEBUG=false CARGO_PROFILE_DEV_STRIP=symbols $(CARGO_LAMBDA) lambda build $(if $(filter dev,$(ENV)),,--release) --x86-64

# --- Build and deploy ---
#   make build <app>          app: api, webhooks, time-triggered
#   make deploy <app> <env>   env: dev, prod
#   make deploy-all           api and webhooks, each to dev and prod
# Each app is a crate whose binary has the same name.
APPS := api webhooks time-triggered
ENVS := dev prod
APP ?= $(firstword $(filter $(APPS),$(MAKECMDGOALS)))
ENV ?= $(firstword $(filter $(ENVS),$(MAKECMDGOALS)))

# The Lambdas of each app in each environment.
# granite-api-stream is the same binary behind a streaming function URL, for
# answers that stream or outlast API Gateway's 30 s (LAMBDA_RESPONSE_STREAMING=1).
# granite-api-stream-dev is its dev twin on the duplicate database.
FUNCTION_api_prod := granite-api granite-api-stream
FUNCTION_api_dev := granite-api-dev granite-api-stream-dev
FUNCTION_webhooks_prod := granite-webhooks
FUNCTION_webhooks_dev := granite-webhooks-dev
FUNCTION_time-triggered_prod := time-triggered
# On the duplicate database and the dev API. Its schedule (process-time-based-dev)
# is disabled: it sends real email to the recipients copied from production.
FUNCTION_time-triggered_dev := time-triggered-dev
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
# An app's Lambdas (api has two) are deployed at once, each from its own copy
# of the binary: cargo-lambda zips the binary into its folder, and two deploys
# writing one zip could upload it half-written. Each one's output is shown
# when all are done.
.PHONY: deploy
deploy:
	@test -n "$(APP)" -a -n "$(ENV)" || { echo "Usage: make deploy <$(APPS)> <$(ENVS)>" >&2; exit 1; }
	@test -n "$(FUNCTION)" || { echo "$(APP) has no $(ENV) Lambda" >&2; exit 1; }
	$(MAKE) build APP=$(APP) ENV=$(ENV)
	@eval "$$(aws configure export-credentials --profile $(AWS_DEPLOY_PROFILE) --format env)" && \
	unset AWS_PROFILE && \
	dir=$$(mktemp -d) && trap 'rm -rf "$$dir"' EXIT && \
	echo "Deploying $(FUNCTION)" && \
	pids= && \
	for function in $(FUNCTION); do \
		mkdir -p "$$dir/$$function/$(APP)" && \
		cp -p "$${CARGO_TARGET_DIR:-target}/lambda/$(APP)/bootstrap" "$$dir/$$function/$(APP)/" && \
		$(CARGO_LAMBDA) lambda deploy \
			--iam-role $(IAM_ROLE) \
			--region $(REGION) \
			--lambda-dir "$$dir/$$function" \
			--binary-name $(APP) \
			$$function > "$$dir/$$function.log" 2>&1 & \
		pids="$$pids $$!"; \
	done && \
	status=0 && \
	for pid in $$pids; do wait $$pid || status=1; done; \
	for function in $(FUNCTION); do echo "$$function:" && cat "$$dir/$$function.log"; done; \
	exit $$status

# api and webhooks, each built and deployed to dev and prod.
.PHONY: deploy-all
deploy-all:
	$(MAKE) deploy APP=api ENV=dev
	$(MAKE) deploy APP=api ENV=prod
	$(MAKE) deploy APP=webhooks ENV=dev
	$(MAKE) deploy APP=webhooks ENV=prod

# --- Local ---
WATCH_BASE := $(CARGO_LAMBDA) lambda watch --release

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
	$(CARGO_LAMBDA) lambda deploy \
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
# Without Docker (e.g. Windows), a MySQL 8 already running on 127.0.0.1:3307
# with root/granite stands in for the container, through its own client.
HAS_DOCKER := $(shell command -v docker 2>/dev/null)
# The MySQL client: on the PATH, else where the Windows installer puts it.
MYSQL_WIN := /c/Program Files/MySQL/MySQL Server 8.0/bin/mysql.exe
MYSQL ?= $(if $(shell command -v mysql 2>/dev/null),mysql,$(if $(shell test -x "$(MYSQL_WIN)" && echo y),"$(MYSQL_WIN)",mysql))
LOCAL_MYSQL := $(if $(HAS_DOCKER),docker exec -i granite-local-mysql mysql,$(MYSQL) -h 127.0.0.1 -P 3307) -uroot -pgranite

.PHONY: api-db-up
api-db-up:
ifneq ($(HAS_DOCKER),)
	docker compose -f docker-compose.local.yml up -d --wait
else
	@$(LOCAL_MYSQL) -e "SELECT 1" >/dev/null 2>&1 || { \
		echo "No Docker, and no MySQL on 127.0.0.1:3307. Start a MySQL 8 there with user root, password granite (on Windows: powershell -File ~/.granite-local-mysql/start.ps1)." >&2; \
		exit 1; }
endif

# Drop, re-migrate and re-seed the local database.
.PHONY: api-db-reset
api-db-reset: api-db-up
	$(LOCAL_MYSQL) -e "DROP DATABASE IF EXISTS $(LOCAL_DB_NAME); CREATE DATABASE $(LOCAL_DB_NAME);"
	DATABASE_URL=$(LOCAL_DB_URL) cargo sqlx migrate run --source migrations
	$(LOCAL_MYSQL) --default-character-set=utf8mb4 $(LOCAL_DB_NAME) < api/seed/local_seed.sql

# Every api crate's tests: the unit tests of api/crates/* and api's
# integration tests (one binary per area, api/tests/<area>/main.rs).
.PHONY: api-test
api-test: api-db-up
	DATABASE_URL=$(LOCAL_DB_URL) cargo test -p 'api*'

# Regenerates api/openapi.json from the route annotations (no database needed).
# Only builds with the `openapi` feature run utoipa; the others serve this file.
.PHONY: api-openapi
api-openapi:
	UPDATE_OPENAPI=1 cargo test -p api --features openapi --test app openapi::generated::spec_file_is_current

# Regenerates webhooks/openapi.json from the lead form route's annotations.
# Only builds with the `openapi` feature run utoipa; the others serve this file.
.PHONY: webhooks-openapi
webhooks-openapi:
	UPDATE_OPENAPI=1 cargo test -p webhooks --features openapi --lib openapi_tests::spec_file_is_current

# Serves http://localhost:9100/lambda-url/api/... through the Lambda runtime emulator.
# A dev build, not --release: an edit rebuilds in seconds, not minutes (the
# dependencies are optimized in the dev profile too, see Cargo.toml).
.PHONY: api-local
api-local:
	test -f api/.env.local || cp api/.env.local.example api/.env.local
	$(CARGO_LAMBDA) lambda watch -p api --bin api -P 9100 --env-file api/.env.local
