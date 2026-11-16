# email-fast-flight

Apache Arrow Flight server for email parsing.

## Quick Start

```bash
# Start the server
PARSERS_FLIGHT_REQUIRE_AUTH=true \
PARSERS_JWT_PUBLIC_KEY_PATH=/run/secrets/parsers-jwt-public.pem \
email-fast-flight --root /path/to/emails --port 50054

# Query with any Arrow Flight client
```

The server exits before binding when authentication is disabled or signing
material is missing. Every retrieval RPC requires
`authorization: Bearer <JWT>` Flight metadata. Prefer an ES256 public-key file;
see the workspace [Flight server authentication
guide](../README.md#arrow-flight-servers) for issuer/audience validation and
the temporary legacy-HS256 migration settings. Do not bake a token, private
key, or shared secret into an image.

## Options

```bash
email-fast-flight --help

Options:
  -r, --root <ROOT>                  Root directory containing email files
  -p, --port <PORT>                  Port to listen on [default: 50054]
  -H, --host <HOST>                  Host to bind to [default: 0.0.0.0]
      --max-attachment-size <SIZE>   Per-attachment decoded MiB [default: 100]
      --max-total-attachment-size <SIZE> Aggregate decoded MiB [default: per-attachment limit]
      --max-message-size <SIZE>      Input/decoded message MiB [default: 128]
      --max-mime-depth <DEPTH>       Nesting depth [default: 64]
      --max-mime-parts <COUNT>       MIME parts/MSG entries [default: 10000]
      --max-header-bytes <BYTES>     Headers per MIME part [default: 262144]
  -h, --help                         Print help
```

## Example

```bash
# Serve emails from /var/mail
PARSERS_FLIGHT_REQUIRE_AUTH=true \
PARSERS_JWT_PUBLIC_KEY_PATH=/run/secrets/parsers-jwt-public.pem \
email-fast-flight --root /var/mail --port 50054

# With custom attachment limit
PARSERS_FLIGHT_REQUIRE_AUTH=true \
PARSERS_JWT_PUBLIC_KEY_PATH=/run/secrets/parsers-jwt-public.pem \
email-fast-flight --root ~/Emails --max-attachment-size 500 --max-message-size 768
```

## License

MIT
