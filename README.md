# Introduction

grante-webhooks is a Rust project that implements an AWS Lambda function in Rust.

## Deploying

uvx cargo-lambda lambda build --release
uvx cargo-lambda lambda deploy --iam-role arn:aws:iam::741448943665:role/cargo-lambda-role-2ed5069c-8882-460d-bdc8-192d9b724756

## Testing

uvx cargo-lambda lambda watch --env-file .env

Connect to internet: ngrok http 9000

Conntect to telegram: 
```
curl -X POST "https://api.telegram.org/bot<token>/setWebhook" \
      -d "url=https://c0bf99f7c2a8.ngrok-free.app" \
      -d "secret_token=fake"
```

When server dies instantly:
1. Find server: sudo lsof -i :9000
2. Kill that server: kill -9 <PID>

## Functions



### Documenso

```
curl -v -X POST \
  'http://127.0.0.1:9000/lambda-url/granite-webhooks/documenso' \
  -H 'Content-Type: application/json' \
  -d '{ "event": "DOCUMENT_CREATED" }'

```
