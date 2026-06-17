"use strict";
const spec = {
    hostName: "td",
    timezone: "UTC",
    locale: "en_US.utf8",
    bootloaderTarget: "/dev/vda",
    rootFsLabel: "td-root",
    rootMount: "/",
    rootFsType: "ext4",
    sshPort: 22,
    sshPasswordAuth: false,
    sshChallengeResponse: false,
    shipGuix: false,
    persistentPaths: [{ tier: "precious", path: "/var/lib/ssh" }],
    generation: null,
};
system(spec);
