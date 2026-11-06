# ADR-0005: Never bundle, fake or bypass Google Play

Status: Accepted, 2026-11-06

## Problem

The product vision includes Google Play. Play and GMS are licensed to certified devices only, and the easy community workarounds are unlawful or violate Google's and Mojang's terms.

## Options

| Option | Assessment |
|---|---|
| GMS licence from Google | The only legitimate route to Play. Uncertain for a small project |
| Bundle GApps | No redistribution right |
| Script user-side GApps installation | Same problem, with extra steps |
| Reuse SDK Play system images | SDK licence: "solely to develop applications", no redistribution |
| Spoof a certified fingerprint | Deceptive. Violates terms |
| Patch entitlement or Integrity checks | Circumvention. Forbidden by the Play Terms |

## Decision

RAX will never:

- ship, download, or script installation of Google Play, Play Services or GApps;
- spoof device identity, certification state, or Integrity verdicts;
- hide root or tampering from apps;
- modify an app to bypass licensing or DRM.

Contributions that do any of this are rejected.

A Play-enabled RAX ships only under a written licence from Google. Without one, RAX runs without Google apps.

## Consequences

- RAX cannot deliver a Play-purchased Minecraft to users unless Google agrees. This is the largest risk to the product.
- The project stays legally defensible and safe to publish.
