# The test CA

The TLS tests use these certificates (D-1011). Every one of them is offline: nothing here is
trusted outside the tests.

| File           | What                                                                             |
| -------------- | -------------------------------------------------------------------------------- |
| `ca.pem`       | The test CA, `CN=Cheby Test CA`, self-signed, `CA:TRUE`                          |
| `leaf.pem`     | The server's certificate, `CN=localhost`, for `DNS:localhost` and `IP:127.0.0.1` |
| `leaf-key.pem` | The leaf's private key (PKCS #8), for the tests' `rustls` servers                |

Both certificates are ECDSA P-256 with SHA-256, valid from 2026-01-01 00:00:00 UTC until
9999-12-31 23:59:59 UTC, so they never expire. The CA's key was deleted after it signed the leaf: a
new leaf needs a new CA.

A debug runtime trusts `ca.pem` besides the roots of `webpki-roots` when `CHEBY_RT_TEST_TLS_ROOT`
names it. A release runtime never reads the variable. The runtime's unit tests build their own
client configuration from these files instead.

## How they were made

With OpenSSL 4.1.0 (`-not_before` and `-not_after` need OpenSSL 3.4 or later), in this directory:

```sh
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out ca-key.pem
openssl req -x509 -new -key ca-key.pem -sha256 -subj "/CN=Cheby Test CA" \
  -not_before 20260101000000Z -not_after 99991231235959Z -set_serial 1 \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign" -out ca.pem
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out leaf-key.pem
openssl req -new -key leaf-key.pem -sha256 -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
  -addext "basicConstraints=critical,CA:FALSE" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=serverAuth" -out leaf.csr
openssl x509 -req -in leaf.csr -copy_extensions copyall -CA ca.pem -CAkey ca-key.pem -sha256 \
  -set_serial 2 -not_before 20260101000000Z -not_after 99991231235959Z -out leaf.pem
rm ca-key.pem leaf.csr
```

The keys are random, so a second run makes other bytes with the same subjects, extensions and
validity. To check a result:

```sh
openssl x509 -in leaf.pem -noout -subject -issuer -dates -ext subjectAltName
openssl verify -CAfile ca.pem leaf.pem
```
