# Introduction

grante-webhooks is a Rust project that implements an AWS Lambda function in Rust.

## Building

To build the project for production, run `cargo lambda build --release`. Remove the `--release` flag to build for development.

Read more about building your lambda function in [the Cargo Lambda documentation](https://www.cargo-lambda.info/commands/build.html).

## Testing

uvx cargo-lambda lambda watch --env-file .env


## Functions

### Documenso

```
curl -v -X POST \
  'http://127.0.0.1:9000/lambda-url/granite-webhooks/documenso' \
  -H 'Content-Type: application/json' \
  -d '{ "event": "DOCUMENT_CREATED" }'

```
