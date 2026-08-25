use crate::ladder::{post_bootstrap_path, POST_BOOTSTRAP_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

pub fn recipe() -> Recipe {
    let sgcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let sbin = "{in:binutils-x86-64-self}/bin";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let curl = "{in:curl-x86-64}";
    let tls = "{in:libressl-x86-64}";
    let zlib = "{in:zlib-x86-64-self}";
    let path = format!("{{tools}}:{sbin}:{}", post_bootstrap_path());

    let mut steps = vec![
        Step::MkDir {
            path: "{root}/archive".into(),
        },
        Step::run(
            "{root}/archive",
            &[
                "{in:binutils-x86-64-self}/bin/ar",
                "x",
                &format!("{curl}/lib/libcurl.a"),
            ],
        ),
        Step::run(
            "{root}/archive",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                "objects=0; found_version=0; \
                 for object in *.o; do \
                     test -f \"$object\" || continue; objects=$((objects+1)); \
                     info=$('{in:binutils-x86-64-self}/bin/readelf' --debug-dump=info \"$object\") || exit 1; \
                     sections=$('{in:binutils-x86-64-self}/bin/readelf' -S \"$object\") || exit 1; \
                     printf '%s\\n' \"$sections\" | grep -Eq '^[[:space:]]*\\[[[:space:]]*[0-9]+\\][[:space:]]+\\.debug_line[[:space:]]' || { echo \"libcurl member $object has no line table\" >&2; exit 1; }; \
                     if '{in:binutils-x86-64-self}/bin/nm' --defined-only \"$object\" | grep -Eq '[[:space:]][TtWw][[:space:]]'; then \
                         printf '%s\\n' \"$info\" | grep -Eq 'DW_AT_comp_dir.*: /td-build/lib$' || { echo \"libcurl code member $object does not use /td-build/lib\" >&2; exit 1; }; \
                     fi; \
                     if printf '%s\\n' \"$info\" | grep -Fq 'version.c'; then found_version=1; fi; \
                     header=$('{in:binutils-x86-64-self}/bin/readelf' -h \"$object\") || exit 1; \
                     printf '%s\\n' \"$header\" | grep -Eq 'Class:[[:space:]]+ELF64' || { echo \"libcurl member $object is not ELF64\" >&2; exit 1; }; \
                     printf '%s\\n' \"$header\" | grep -Eq 'Machine:[[:space:]]+Advanced Micro Devices X86-64' || { echo \"libcurl member $object is not x86-64\" >&2; exit 1; }; \
                     if printf '%s\\n' \"$info\" | grep -Eq 'guix-build|/gnu/store|/td/store|/td-build-root|/td-input'; then echo \"libcurl member $object retains a noncanonical build path\" >&2; exit 1; fi; \
                 done; \
                 test \"$objects\" -gt 0 || { echo 'libcurl.a has no objects' >&2; exit 1; }; \
                 test \"$found_version\" = 1 || { echo 'libcurl.a lacks version.c' >&2; exit 1; }; \
                 '{in:binutils-x86-64-self}/bin/nm' -u *.o | grep -Eq '[[:space:]]U[[:space:]]+clock_gettime$' || { echo 'libcurl does not use the monotonic clock' >&2; exit 1; }",
            ],
        )
        .env("PATH", &path),
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                "if grep -a -Eq '/gnu/store|/td/store|/td-input|/td-build-root' '{in:curl-x86-64}/lib/libcurl.a'; then echo 'libcurl archive retains a noncanonical store reference' >&2; exit 1; fi",
            ],
        )
        .env("PATH", &path),
    ];
    steps.push(Step::WriteFile {
        path: "{root}/curl-test.c".into(),
        content: r#"#include <curl/curl.h>
#include <openssl/evp.h>
#include <openssl/pem.h>
#include <openssl/rsa.h>
#include <openssl/ssl.h>
#include <openssl/x509v3.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

struct response_body {
    char data[64];
    size_t length;
};

struct server_state {
    SSL_CTX *context;
    int listener;
    int require_http;
    int ok;
};

static int add_extension(X509 *certificate, int nid, const char *value) {
    X509_EXTENSION *extension = X509V3_EXT_conf_nid(NULL, NULL, nid, value);
    if (extension == NULL)
        return 0;
    int result = X509_add_ext(certificate, extension, -1);
    X509_EXTENSION_free(extension);
    return result == 1;
}

static int make_identity(const char *hostname, EVP_PKEY **key_out,
        X509 **certificate_out) {
    EVP_PKEY_CTX *keygen = EVP_PKEY_CTX_new_id(EVP_PKEY_RSA, NULL);
    EVP_PKEY *key = NULL;
    X509 *certificate = NULL;
    if (keygen == NULL || EVP_PKEY_keygen_init(keygen) <= 0 ||
        EVP_PKEY_CTX_set_rsa_keygen_bits(keygen, 2048) <= 0 ||
        EVP_PKEY_keygen(keygen, &key) <= 0)
        goto fail;
    certificate = X509_new();
    if (certificate == NULL || X509_set_version(certificate, 2) != 1 ||
        ASN1_INTEGER_set(X509_get_serialNumber(certificate), 1) != 1 ||
        X509_gmtime_adj(X509_get_notBefore(certificate), -60) == NULL ||
        X509_gmtime_adj(X509_get_notAfter(certificate), 86400) == NULL ||
        X509_set_pubkey(certificate, key) != 1)
        goto fail;
    X509_NAME *name = X509_get_subject_name(certificate);
    if (name == NULL || X509_NAME_add_entry_by_txt(name, "CN", MBSTRING_ASC,
            (const unsigned char *)hostname, -1, -1, 0) != 1 ||
        X509_set_issuer_name(certificate, name) != 1)
        goto fail;
    char subject_alt_name[128];
    if (snprintf(subject_alt_name, sizeof(subject_alt_name), "DNS:%s",
            hostname) < 0 ||
        !add_extension(certificate, NID_subject_alt_name, subject_alt_name) ||
        !add_extension(certificate, NID_basic_constraints, "critical,CA:TRUE") ||
        !add_extension(certificate, NID_key_usage,
            "critical,digitalSignature,keyEncipherment,keyCertSign") ||
        !add_extension(certificate, NID_ext_key_usage, "serverAuth") ||
        X509_sign(certificate, key, EVP_sha256()) <= 0)
        goto fail;
    EVP_PKEY_CTX_free(keygen);
    *key_out = key;
    *certificate_out = certificate;
    return 1;
fail:
    X509_free(certificate);
    EVP_PKEY_free(key);
    EVP_PKEY_CTX_free(keygen);
    return 0;
}

static int certificate_pem(X509 *certificate, char *output, size_t capacity,
        size_t *length_out) {
    BIO *bio = BIO_new(BIO_s_mem());
    if (bio == NULL || PEM_write_bio_X509(bio, certificate) != 1) {
        BIO_free(bio);
        return 0;
    }
    int length = BIO_read(bio, output, (int)capacity);
    BIO_free(bio);
    if (length <= 0)
        return 0;
    *length_out = (size_t)length;
    return 1;
}

static int open_listener(char *path, size_t path_capacity) {
    int length = snprintf(path, path_capacity, "curl-test-%ld.sock",
        (long)getpid());
    if (length < 0 || (size_t)length >= path_capacity)
        return -1;
    int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    if (listener < 0)
        return -1;
    struct sockaddr_un address;
    memset(&address, 0, sizeof(address));
    address.sun_family = AF_UNIX;
    if (strlen(path) >= sizeof(address.sun_path)) {
        close(listener);
        return -1;
    }
    memcpy(address.sun_path, path, strlen(path) + 1);
    unlink(path);
    if (bind(listener, (struct sockaddr *)&address, sizeof(address)) != 0 ||
        listen(listener, 1) != 0) {
        close(listener);
        unlink(path);
        return -1;
    }
    return listener;
}

static void *serve_https(void *opaque) {
    struct server_state *state = opaque;
    int connection = accept(state->listener, NULL, NULL);
    if (connection < 0)
        return NULL;
    SSL *ssl = SSL_new(state->context);
    if (ssl == NULL || SSL_set_fd(ssl, connection) != 1) {
        SSL_free(ssl);
        close(connection);
        return NULL;
    }
    int handshake = SSL_accept(ssl);
    if (!state->require_http) {
        state->ok = 1;
    } else if (handshake == 1) {
        char request[2048];
        int length = SSL_read(ssl, request, sizeof(request) - 1);
        if (length > 0) {
            request[length] = 0;
            static const char response[] =
                "HTTP/1.1 200 OK\r\n"
                "Content-Length: 11\r\n"
                "Connection: close\r\n\r\n"
                "td-curl-ok\n";
            if (strstr(request, "GET / HTTP/1.1\r\n") != NULL &&
                SSL_write(ssl, response, sizeof(response) - 1) ==
                    (int)(sizeof(response) - 1))
                state->ok = 1;
        }
    }
    SSL_free(ssl);
    close(connection);
    return NULL;
}

static size_t capture_body(char *data, size_t size, size_t count, void *opaque) {
    struct response_body *body = opaque;
    size_t bytes = size * count;
    if (bytes > sizeof(body->data) - body->length - 1)
        return 0;
    memcpy(body->data + body->length, data, bytes);
    body->length += bytes;
    body->data[body->length] = 0;
    return bytes;
}

static CURLcode perform_transfer(SSL_CTX *server_context, const char *hostname,
        const char *ca_pem, size_t ca_length, int require_http,
        struct response_body *body, int *server_ok) {
    char socket_path[sizeof(((struct sockaddr_un *)0)->sun_path)];
    int listener = open_listener(socket_path, sizeof(socket_path));
    if (listener < 0)
        return CURLE_COULDNT_CONNECT;
    char url[256];
    if (snprintf(url, sizeof(url), "https://%s/", hostname) < 0) {
        close(listener);
        unlink(socket_path);
        return CURLE_FAILED_INIT;
    }
    CURL *easy = curl_easy_init();
    if (easy == NULL) {
        curl_easy_cleanup(easy);
        close(listener);
        unlink(socket_path);
        return CURLE_OUT_OF_MEMORY;
    }
    struct curl_blob ca_blob = {(void *)ca_pem, ca_length, CURL_BLOB_COPY};
#define SETOPT(option, value) \
    do { \
        CURLcode option_result = curl_easy_setopt(easy, option, value); \
        if (option_result != CURLE_OK) { \
            curl_easy_cleanup(easy); \
            close(listener); \
            unlink(socket_path); \
            return option_result; \
        } \
    } while (0)
    SETOPT(CURLOPT_URL, url);
    SETOPT(CURLOPT_UNIX_SOCKET_PATH, socket_path);
    SETOPT(CURLOPT_PROXY, "");
    SETOPT(CURLOPT_CAINFO_BLOB, &ca_blob);
    SETOPT(CURLOPT_SSL_VERIFYPEER, 1L);
    SETOPT(CURLOPT_SSL_VERIFYHOST, 2L);
    SETOPT(CURLOPT_NOSIGNAL, 1L);
    SETOPT(CURLOPT_CONNECTTIMEOUT_MS, 3000L);
    SETOPT(CURLOPT_TIMEOUT_MS, 5000L);
    SETOPT(CURLOPT_HTTP_VERSION, CURL_HTTP_VERSION_1_1);
    SETOPT(CURLOPT_WRITEFUNCTION, capture_body);
    SETOPT(CURLOPT_WRITEDATA, body);
#undef SETOPT
    struct server_state state = {server_context, listener, require_http, 0};
    pthread_t thread;
    if (pthread_create(&thread, NULL, serve_https, &state) != 0) {
        curl_easy_cleanup(easy);
        close(listener);
        unlink(socket_path);
        return CURLE_FAILED_INIT;
    }
    CURLcode result = curl_easy_perform(easy);
    shutdown(listener, SHUT_RDWR);
    close(listener);
    pthread_join(thread, NULL);
    unlink(socket_path);
    curl_easy_cleanup(easy);
    *server_ok = state.ok;
    return result;
}

static const char *shown(const char *value) {
    return value == NULL ? "<null>" : value;
}

static int fail_text(const char *label, const char *expected,
        const char *actual, int code) {
    fprintf(stderr, "%s: expected %s, got %s\n", label, expected,
        shown(actual));
    return code;
}

int main(void) {
    signal(SIGPIPE, SIG_IGN);
    if (curl_global_init(CURL_GLOBAL_DEFAULT) != CURLE_OK) {
        fputs("curl global initialization failed\n", stderr);
        return 1;
    }
    curl_version_info_data *version = curl_version_info(CURLVERSION_NOW);
    if (version == NULL || strcmp(version->version, "8.21.0") != 0)
        return fail_text("curl version", "8.21.0",
            version == NULL ? NULL : version->version, 2);
    const int expected_features = CURL_VERSION_IPV6 | CURL_VERSION_SSL |
        CURL_VERSION_LIBZ | CURL_VERSION_ASYNCHDNS | CURL_VERSION_LARGEFILE |
        CURL_VERSION_UNIX_SOCKETS | CURL_VERSION_HTTPS_PROXY |
        CURL_VERSION_THREADSAFE;
    if (version->features != expected_features) {
        fprintf(stderr, "curl feature mask: expected 0x%x, got 0x%x\n",
            expected_features, version->features);
        return 3;
    }
    static const char *const expected_feature_names[] = {
        "AsynchDNS", "HTTPS-proxy", "IPv6", "Largefile", "libz", "SSL",
        "threadsafe", "UnixSockets", NULL
    };
    for (size_t index = 0;; ++index) {
        const char *actual = version->feature_names[index];
        const char *expected = expected_feature_names[index];
        if (actual == NULL || expected == NULL) {
            if (actual != expected) {
                fprintf(stderr, "curl feature name %zu: expected %s, got %s\n",
                    index, shown(expected), shown(actual));
                return 4;
            }
            break;
        }
        if (strcmp(actual, expected) != 0) {
            fprintf(stderr, "curl feature name %zu: expected %s, got %s\n",
                index, expected, actual);
            return 4;
        }
    }
    if (version->ssl_version == NULL ||
        strstr(version->ssl_version, "LibreSSL/4.3.2") == NULL)
        return fail_text("curl TLS backend", "a LibreSSL/4.3.2 value",
            version->ssl_version, 5);
    if (version->libz_version == NULL ||
        strcmp(version->libz_version, "1.3.1") != 0)
        return fail_text("curl zlib version", "1.3.1",
            version->libz_version, 6);
    if (version->cainfo == NULL || strcmp(version->cainfo,
            "/etc/ssl/certs/ca-certificates.crt") != 0 ||
        version->capath != NULL) {
        fprintf(stderr, "curl CA paths: expected bundle %s and no directory, got %s and %s\n",
            "/etc/ssl/certs/ca-certificates.crt", shown(version->cainfo),
            shown(version->capath));
        return 7;
    }
    unsigned protocols = 0;
    for (const char *const *item = version->protocols;
            item != NULL && *item != NULL; ++item) {
        if (strcmp(*item, "http") == 0)
            protocols |= 1;
        else if (strcmp(*item, "https") == 0)
            protocols |= 2;
        else {
            fprintf(stderr, "curl protocol set contains unexpected %s\n", *item);
            return 8;
        }
    }
    if (protocols != 3) {
        fprintf(stderr, "curl protocol mask: expected 3, got %u\n", protocols);
        return 9;
    }

    EVP_PKEY *server_key = NULL;
    EVP_PKEY *unrelated_key = NULL;
    X509 *server_certificate = NULL;
    X509 *unrelated_certificate = NULL;
    if (!make_identity("localhost", &server_key, &server_certificate) ||
        !make_identity("unrelated.example", &unrelated_key,
            &unrelated_certificate))
        return 10;
    SSL_CTX *server_context = SSL_CTX_new(TLS_server_method());
    if (server_context == NULL ||
        SSL_CTX_use_certificate(server_context, server_certificate) != 1 ||
        SSL_CTX_use_PrivateKey(server_context, server_key) != 1 ||
        SSL_CTX_check_private_key(server_context) != 1)
        return 11;
    char trusted_pem[8192];
    char unrelated_pem[8192];
    size_t trusted_length = 0;
    size_t unrelated_length = 0;
    if (!certificate_pem(server_certificate, trusted_pem, sizeof(trusted_pem),
            &trusted_length) ||
        !certificate_pem(unrelated_certificate, unrelated_pem,
            sizeof(unrelated_pem), &unrelated_length))
        return 12;

    struct response_body body = {{0}, 0};
    int server_ok = 0;
    CURLcode result = perform_transfer(server_context, "localhost", trusted_pem,
        trusted_length, 1, &body, &server_ok);
    if (result != CURLE_OK || !server_ok ||
        strcmp(body.data, "td-curl-ok\n") != 0) {
        fprintf(stderr, "positive HTTPS failed: curl=%d (%s), server=%d, body=%s\n",
            (int)result, curl_easy_strerror(result), server_ok, body.data);
        return 13;
    }

    memset(&body, 0, sizeof(body));
    server_ok = 0;
    result = perform_transfer(server_context, "wrong.example", trusted_pem,
        trusted_length, 0, &body, &server_ok);
    if (result != CURLE_PEER_FAILED_VERIFICATION || !server_ok) {
        fprintf(stderr, "hostname rejection failed: curl=%d (%s), server=%d\n",
            (int)result, curl_easy_strerror(result), server_ok);
        return 14;
    }

    memset(&body, 0, sizeof(body));
    server_ok = 0;
    result = perform_transfer(server_context, "localhost", unrelated_pem,
        unrelated_length, 0, &body, &server_ok);
    if (result != CURLE_PEER_FAILED_VERIFICATION || !server_ok) {
        fprintf(stderr, "trust rejection failed: curl=%d (%s), server=%d\n",
            (int)result, curl_easy_strerror(result), server_ok);
        return 15;
    }

    SSL_CTX_free(server_context);
    X509_free(server_certificate);
    X509_free(unrelated_certificate);
    EVP_PKEY_free(server_key);
    EVP_PKEY_free(unrelated_key);
    curl_global_cleanup();
    puts("curl 8.21.0 verified HTTPS and rejected hostname/trust failures");
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
                &format!("-I{curl}/include"),
                &format!("-I{tls}/include"),
                "-o",
                "{root}/curl-test",
                "{root}/curl-test.c",
                &format!("{curl}/lib/libcurl.a"),
                &format!("{tls}/lib/libssl.a"),
                &format!("{tls}/lib/libcrypto.a"),
                &format!("{zlib}/lib/libz.a"),
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
                "actual=$('{root}/curl-test'); status=$?; \
                 if test \"$status\" -ne 0; then echo \"curl HTTPS probe failed with status $status\" >&2; exit \"$status\"; fi; \
                 test \"$actual\" = 'curl 8.21.0 verified HTTPS and rejected hostname/trust failures' || { echo \"unexpected curl result: $actual\" >&2; exit 1; }",
            ],
        )
        .env("PATH", &path),
    );
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: curl 8.21.0 performs verified local-socket HTTPS and rejects hostname and trust failures\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("curl-x86-64-test", "1.0")
        .native_inputs(&[
            "curl-x86-64",
            "libressl-x86-64",
            "zlib-x86-64-self",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check curl-x86-64-test: build static HTTP(S)-only libcurl and validate verified local-socket HTTPS"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run curl-x86-64-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::recipe;

    #[test]
    fn validation_uses_only_the_final_https_closure() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs.as_deref(),
            Some(
                [
                    "curl-x86-64",
                    "libressl-x86-64",
                    "zlib-x86-64-self",
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
