use crate::ladder::{post_bootstrap_path, POST_BOOTSTRAP_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// Behavioral validation for the static TLS foundation. The test links a fresh
// target executable against only the installed libssl.a + libcrypto.a, creates
// a fresh RSA identity, drives an in-memory client/server TLS handshake, verifies
// the localhost certificate, rejects a hostname mismatch, and exchanges data.
// This catches a partial archive, missing compatibility object, broken public
// headers, or an accidental link against ambient TLS before curl inherits it.
pub fn recipe() -> Recipe {
    let sgcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let sbin = "{in:binutils-x86-64-self}/bin";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let tls = "{in:libressl-x86-64}";
    let path = format!("{{tools}}:{sbin}:{}", post_bootstrap_path());

    let mut steps = Vec::new();
    for (name, archive, source, comp_dir_pattern, compat_members) in [
        (
            "crypto",
            "libcrypto.a",
            "crypto_init.c",
            "/td-build/crypto$",
            "freezero.o recallocarray.o timingsafe_memcmp.o",
        ),
        (
            "ssl",
            "libssl.a",
            "ssl_init.c",
            "/td-build/(ssl|crypto)$",
            "",
        ),
    ] {
        steps.push(Step::MkDir {
            path: format!("{{root}}/archive/{name}"),
        });
        steps.push(Step::run(
            &format!("{{root}}/archive/{name}"),
            &[
                "{in:binutils-x86-64-self}/bin/ar",
                "x",
                &format!("{tls}/lib/{archive}"),
            ],
        ));
        steps.push(
            Step::run(
                &format!("{{root}}/archive/{name}"),
                &[
                    POST_BOOTSTRAP_SH,
                    "-c",
                    &format!(
                        "found_source=0; objects=0; \
                         for object in *.o; do \
                             test -f \"$object\" || continue; objects=$((objects+1)); \
                             info=$('{{in:binutils-x86-64-self}}/bin/readelf' --debug-dump=info \"$object\") || exit 1; \
                             sections=$('{{in:binutils-x86-64-self}}/bin/readelf' -S \"$object\") || exit 1; \
                             printf '%s\\n' \"$sections\" | grep -Eq '^[[:space:]]*\\[[[:space:]]*[0-9]+\\][[:space:]]+\\.debug_line[[:space:]]' || {{ echo \"LibreSSL {archive} member $object has no line table\" >&2; exit 1; }}; \
                             printf '%s\\n' \"$info\" | grep -Eq 'DW_AT_comp_dir.*: {comp_dir_pattern}' || {{ echo \"LibreSSL {archive} member $object does not use a canonical LibreSSL source root\" >&2; exit 1; }}; \
                             if printf '%s\\n' \"$info\" | grep -Fq '{source}'; then found_source=1; fi; \
                             header=$('{{in:binutils-x86-64-self}}/bin/readelf' -h \"$object\") || exit 1; \
                             printf '%s\\n' \"$header\" | grep -Eq 'Class:[[:space:]]+ELF64' || {{ echo \"LibreSSL {archive} member $object is not ELF64\" >&2; exit 1; }}; \
                             printf '%s\\n' \"$header\" | grep -Eq 'Machine:[[:space:]]+Advanced Micro Devices X86-64' || {{ echo \"LibreSSL {archive} member $object is not x86-64\" >&2; exit 1; }}; \
                             if printf '%s\\n' \"$info\" | grep -Eq 'guix-build|/gnu/store|/td-build-root'; then echo \"LibreSSL {archive} member $object retains a foreign build path\" >&2; exit 1; fi; \
                         done; \
                         test \"$objects\" -gt 0 || {{ echo 'LibreSSL {archive} has no objects' >&2; exit 1; }}; \
                         test \"$found_source\" = 1 || {{ echo 'LibreSSL {archive} lacks {source}' >&2; exit 1; }}; \
                         for required in {compat_members}; do \
                             test -f \"$required\" || {{ echo \"LibreSSL {archive} lacks compatibility member $required\" >&2; exit 1; }}; \
                         done"
                    ),
                ],
            )
            .env("PATH", &path),
        );
    }
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                "for archive in '{in:libressl-x86-64}/lib/libcrypto.a' '{in:libressl-x86-64}/lib/libssl.a'; do \
                     if grep -a -Fq '/gnu/store' \"$archive\"; then echo 'LibreSSL archive retains a foreign store reference' >&2; exit 1; fi; \
                 done",
            ],
        )
        .env("PATH", &path),
    );
    steps.push(Step::WriteFile {
        path: "{root}/tls-test.c".into(),
        content: r#"#include <openssl/crypto.h>
#include <openssl/evp.h>
#include <openssl/opensslv.h>
#include <openssl/rand.h>
#include <openssl/rsa.h>
#include <openssl/sha.h>
#include <openssl/ssl.h>
#include <openssl/x509v3.h>
#include <stdio.h>
#include <string.h>

static int drive_handshake(SSL *client, SSL *server) {
    int client_done = 0;
    int server_done = 0;
    for (int round = 0; round < 10000; ++round) {
        if (!client_done) {
            int result = SSL_do_handshake(client);
            if (result == 1) {
                client_done = 1;
            } else {
                int error = SSL_get_error(client, result);
                if (error != SSL_ERROR_WANT_READ && error != SSL_ERROR_WANT_WRITE)
                    return 0;
            }
        }
        if (!server_done) {
            int result = SSL_do_handshake(server);
            if (result == 1) {
                server_done = 1;
            } else {
                int error = SSL_get_error(server, result);
                if (error != SSL_ERROR_WANT_READ && error != SSL_ERROR_WANT_WRITE)
                    return 0;
            }
        }
        if (client_done && server_done)
            return 1;
    }
    return 0;
}

static int attach_pair(SSL *client, SSL *server, const char *hostname) {
    BIO *client_bio = NULL;
    BIO *server_bio = NULL;
    if (BIO_new_bio_pair(&client_bio, 0, &server_bio, 0) != 1)
        return 0;
    SSL_set_bio(client, client_bio, client_bio);
    SSL_set_bio(server, server_bio, server_bio);
    SSL_set_connect_state(client);
    SSL_set_accept_state(server);
    if (SSL_set_tlsext_host_name(client, hostname) != 1)
        return 0;
    return X509_VERIFY_PARAM_set1_host(SSL_get0_param(client), hostname, 0) == 1;
}

static int make_identity(EVP_PKEY **key_out, X509 **cert_out) {
    EVP_PKEY_CTX *keygen = EVP_PKEY_CTX_new_id(EVP_PKEY_RSA, NULL);
    EVP_PKEY *key = NULL;
    X509 *cert = NULL;
    X509_EXTENSION *san = NULL;
    if (keygen == NULL || EVP_PKEY_keygen_init(keygen) <= 0 ||
        EVP_PKEY_CTX_set_rsa_keygen_bits(keygen, 2048) <= 0 ||
        EVP_PKEY_keygen(keygen, &key) <= 0)
        goto fail;
    cert = X509_new();
    if (cert == NULL || X509_set_version(cert, 2) != 1 ||
        ASN1_INTEGER_set(X509_get_serialNumber(cert), 1) != 1 ||
        X509_gmtime_adj(X509_get_notBefore(cert), -60) == NULL ||
        X509_gmtime_adj(X509_get_notAfter(cert), 86400) == NULL ||
        X509_set_pubkey(cert, key) != 1)
        goto fail;
    X509_NAME *name = X509_get_subject_name(cert);
    if (name == NULL ||
        X509_NAME_add_entry_by_txt(name, "CN", MBSTRING_ASC,
            (const unsigned char *)"localhost", -1, -1, 0) != 1 ||
        X509_set_issuer_name(cert, name) != 1)
        goto fail;
    san = X509V3_EXT_conf_nid(NULL, NULL, NID_subject_alt_name, "DNS:localhost");
    if (san == NULL || X509_add_ext(cert, san, -1) != 1 ||
        X509_sign(cert, key, EVP_sha256()) <= 0)
        goto fail;
    X509_EXTENSION_free(san);
    EVP_PKEY_CTX_free(keygen);
    *key_out = key;
    *cert_out = cert;
    return 1;
fail:
    X509_EXTENSION_free(san);
    X509_free(cert);
    EVP_PKEY_free(key);
    EVP_PKEY_CTX_free(keygen);
    return 0;
}

int main(void) {
    const char *version = OpenSSL_version(OPENSSL_VERSION);
    if (strcmp(version, LIBRESSL_VERSION_TEXT) != 0)
        return 1;
    static const unsigned char sha256_abc[SHA256_DIGEST_LENGTH] = {
        0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea,
        0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22, 0x23,
        0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c,
        0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00, 0x15, 0xad
    };
    unsigned char digest[SHA256_DIGEST_LENGTH];
    unsigned char random_bytes[32];
    if (SHA256((const unsigned char *)"abc", 3, digest) == NULL ||
        memcmp(digest, sha256_abc, sizeof(digest)) != 0 ||
        RAND_bytes(random_bytes, sizeof(random_bytes)) != 1)
        return 2;
    EVP_PKEY *key = NULL;
    X509 *cert = NULL;
    if (!make_identity(&key, &cert))
        return 3;
    SSL_CTX *client_ctx = SSL_CTX_new(TLS_client_method());
    SSL_CTX *server_ctx = SSL_CTX_new(TLS_server_method());
    if (client_ctx == NULL || server_ctx == NULL)
        return 4;
    if (SSL_CTX_use_certificate(server_ctx, cert) != 1 ||
        SSL_CTX_use_PrivateKey(server_ctx, key) != 1 ||
        SSL_CTX_check_private_key(server_ctx) != 1)
        return 5;
    if (X509_STORE_add_cert(SSL_CTX_get_cert_store(client_ctx), cert) != 1)
        return 6;
    SSL_CTX_set_verify(client_ctx, SSL_VERIFY_PEER, NULL);

    SSL *client = SSL_new(client_ctx);
    SSL *server = SSL_new(server_ctx);
    if (client == NULL || server == NULL ||
        !attach_pair(client, server, "localhost") ||
        !drive_handshake(client, server))
        return 7;
    if (SSL_get_verify_result(client) != X509_V_OK)
        return 8;
    const char request[] = "td-libre-tls";
    char received[sizeof(request)] = {0};
    if (SSL_write(client, request, sizeof(request)) != (int)sizeof(request) ||
        SSL_read(server, received, sizeof(received)) != (int)sizeof(received) ||
        memcmp(request, received, sizeof(request)) != 0)
        return 9;
    if (SSL_get_current_cipher(client) == NULL ||
        strncmp(SSL_get_version(client), "TLSv", 4) != 0)
        return 10;
    SSL_free(client);
    SSL_free(server);

    client = SSL_new(client_ctx);
    server = SSL_new(server_ctx);
    if (client == NULL || server == NULL ||
        !attach_pair(client, server, "wrong.example") ||
        drive_handshake(client, server) ||
        SSL_get_verify_result(client) != X509_V_ERR_HOSTNAME_MISMATCH)
        return 11;

    SSL_free(client);
    SSL_free(server);
    SSL_CTX_free(client_ctx);
    SSL_CTX_free(server_ctx);
    X509_free(cert);
    EVP_PKEY_free(key);
    printf("%s verified TLS handshake OK\n", version);
    return 0;
}
"#
        .into(),
        exec: false,
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                sgcc,
                "-isystem",
                &format!("{xglibc}/include"),
                "-B",
                &format!("{sbin}/"),
                "-B",
                &format!("{xglibc}/lib"),
                "-L",
                &format!("{xglibc}/lib"),
                "-static-libgcc",
                "-Wl,--dynamic-linker",
                &format!("-Wl,{xglibc}/lib/ld-linux-x86-64.so.2"),
                "-Wl,--enable-new-dtags",
                "-Wl,-rpath",
                &format!("-Wl,{xglibc}/lib"),
                &format!("-I{tls}/include"),
                "-o",
                "{root}/tls-test",
                "{root}/tls-test.c",
                &format!("{tls}/lib/libssl.a"),
                &format!("{tls}/lib/libcrypto.a"),
                "-pthread",
            ],
        )
        .env("PATH", &path),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                "actual=$('{root}/tls-test') || { echo 'LibreSSL verified-handshake probe failed' >&2; exit 1; }; \
                 [ \"$actual\" = 'LibreSSL 4.3.2 verified TLS handshake OK' ] || { echo \"unexpected LibreSSL result: $actual\" >&2; exit 1; }",
            ],
        )
        .env("PATH", &path),
    );
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: LibreSSL 4.3.2 performs an in-memory TLS handshake, verifies localhost, and rejects a hostname mismatch\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("libressl-x86-64-test", "1.0")
        .native_inputs(&[
            "libressl-x86-64",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check libressl-x86-64-test: build LibreSSL from source with td's final native toolchain and create a TLS client context from its static archives"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run libressl-x86-64-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::recipe;

    #[test]
    fn validation_uses_only_the_final_tls_and_toolchain_outputs() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs.as_deref(),
            Some(
                [
                    "libressl-x86-64",
                    "gcc-x86-64-self",
                    "binutils-x86-64-self",
                    "glibc-x86-64",
                    "busybox-x86-64",
                ]
                .map(str::to_string)
                .as_slice()
            )
        );
        assert!(recipe.inputs.is_none());
    }
}
