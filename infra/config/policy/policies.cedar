// JARVIS Cedar ABAC policies.
//
// All *.cedar files in this directory are concatenated (sorted by file name)
// and loaded at startup; entity data may be supplied via entities.json in the
// same directory.
//
// Runtime entity UIDs use the `jarvis` namespace, e.g.
//   jarvis::Principal::"jarvis_admin"
//   jarvis::Action::"execute"
//   jarvis::Resource::"tool:git.commit"
//
// Cedar is deny-by-default: any request not matched by a permit is denied.
// The Casbin RBAC fast path (casbin_policy.csv) is evaluated first, so these
// policies only need to cover the ABAC deep path.

// Admin principal: full access.
permit (
    principal == jarvis::Principal::"jarvis_admin",
    action,
    resource
);

// System principal: internal operations (backup, deploy, health checks).
permit (
    principal == jarvis::Principal::"jarvis_system",
    action,
    resource
);
