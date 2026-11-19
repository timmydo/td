//! Public verification vectors from RFC 8463, Appendix A.
pub const RSA_TXT: &str = "v=DKIM1; k=rsa; p=MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDkHlOQoBTzWRiGs5V6NpP3idY6Wk08a5qhdR6wy5bdOKb2jLQiY/J16JYi0Qvx/byYzCNb3W91y3FutACDfzwQ/BC/e/8uBsCR+yz1Lxj+PL6lHvqMKrM3rG4hstT5QjvHO9PzoxZyVYLzBfO2EeC3Ip3G+2kryOTIKT+l/K4w3QIDAQAB";
pub const ED_TXT: &str = "v=DKIM1; k=ed25519; p=11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
pub const MESSAGE: &[u8] = concat!(
    "DKIM-Signature: v=1; a=ed25519-sha256; c=relaxed/relaxed;\r\n",
    " d=football.example.com; i=@football.example.com;\r\n",
    " q=dns/txt; s=brisbane; t=1528637909; h=from : to :\r\n",
    " subject : date : message-id : from : subject : date;\r\n",
    " bh=2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=;\r\n",
    " b=/gCrinpcQOoIfuHNQIbq4pgh9kyIK3AQUdt9OdqQehSwhEIug4D11Bus\r\n",
    " Fa3bT3FY5OsU7ZbnKELq+eXdp1Q1Dw==\r\n",
    "DKIM-Signature: v=1; a=rsa-sha256; c=relaxed/relaxed;\r\n",
    " d=football.example.com; i=@football.example.com;\r\n",
    " q=dns/txt; s=test; t=1528637909; h=from : to : subject :\r\n",
    " date : message-id : from : subject : date;\r\n",
    " bh=2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=;\r\n",
    " b=F45dVWDfMbQDGHJFlXUNB2HKfbCeLRyhDXgFpEL8GwpsRe0IeIixNTe3\r\n",
    " DhCVlUrSjV4BwcVcOF6+FF3Zo9Rpo1tFOeS9mPYQTnGdaSGsgeefOsk2Jz\r\n",
    " dA+L10TeYt9BgDfQNZtKdN1WO//KgIqXP7OdEFE4LjFYNcUxZQ4FADY+8=\r\n",
    "From: Joe SixPack <joe@football.example.com>\r\n",
    "To: Suzie Q <suzie@shopping.example.net>\r\n",
    "Subject: Is dinner ready?\r\n",
    "Date: Fri, 11 Jul 2003 21:00:37 -0700 (PDT)\r\n",
    "Message-ID: <20030712040037.46341.5F8J@football.example.com>\r\n",
    "\r\n",
    "Hi.\r\n",
    "\r\n",
    "We lost the game.  Are you hungry yet?\r\n",
    "\r\n",
    "Joe.\r\n",
    "\r\n",
)
.as_bytes();
