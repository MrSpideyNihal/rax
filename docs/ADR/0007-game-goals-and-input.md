# ADR-0007: Free Fire and BGMI goals, and the key-mapping rules

Status: Accepted, 2026-11-06. Amends [ADR-0004](0004-x86-64-guest-first.md) by recording a known gap.

## Problem

The project owner added Free Fire and BGMI as goals, with reliable keyboard and mouse play. These games have no keyboard support of their own, so physical input must become touches. They also differ from Minecraft in ways that affect feasibility.

## What the research found

None of this has been tested. It comes from listings and secondary reports, and no official emulator policy from either publisher was found.

| | BGMI | Free Fire |
|---|---|---|
| Android builds | Current releases listed as `arm64-v8a` only | Not confirmed |
| ARM translation | **Required** on x86, and ADR-0004 has no legal translator | Unknown |
| Emulator rules | Reported as banned from competitive events. Enforcement is account-level | Reported separate matchmaking pool, off Ranked and Weapon leaderboards from August 2026 |
| Ban rules | No official emulator text found | Official rules ban unauthorized tools and modified clients |

Sources are listed below.

## Decisions

### Goals

Free Fire and BGMI are compatibility goals. They are not promises. **BGMI is blocked** until a legal ARM translation path exists, and no such path has been found.

### RAX will not hide from anti-cheat

RAX will never spoof device identity, hide that it is virtualized, patch a game, or interfere with anti-cheat. If a game rejects RAX, that result is accepted and documented. The same applies to [ADR-0005](0005-google-play-policy.md).

Users must follow each game's rules, and their accounts carry the risk. RAX docs will say so plainly.

### No macros

Key mapping is one physical action to one direct touch action. RAX will not add recoil control, aim assistance, auto-fire, turbo, timed sequences, or anything that plays for the user. Beyond fairness, these are the kind of "non-official programs that give an edge" that publishers ban.

The design enforces this: `rax-input` has no clock, so it cannot produce input by itself.

### Keyboard and mouse must stay independent

The owner's requirement: connecting a mouse must never disconnect a keyboard, or the reverse.

- Keyboard and mouse state are tracked separately.
- Losing one device releases only that device's held controls.
- A returning device works immediately, with no mode switch.
- Losing window focus releases everything, so no key sticks.
- The guest will see one stable virtual touch device, which does not change when host devices come and go.

## Consequences

- `crates/rax-input` is built and tested without Android. It is not connected to any guest or host device yet.
- Profile positions for real games cannot be set until those games run. The included profile is an untested example.
- Android-side touch handling is unproven. Type B multi-touch needs testing on the chosen Android version.

## Sources

[BGMI 4.6.0 listing](https://www.apkmirror.com/apk/krafton-inc/battlegrounds-mobile-india/bgmi-fps-battle-royale-4-6-0-release/),
[BGMI on Windows, 2026](https://windowsforum.com/news/bgmi-on-windows-in-2026-emulator-vs-screen-mirroring-safety-and-setup.397531/),
[Free Fire account ban rules](https://support-freefireind.garena.com/ind/articles/ind_account_ban),
[Free Fire emulator leaderboards](https://www.talkesport.com/news/free-fire-max-bans-emulator-players-ranked-weapon-leaderboards/),
[Free Fire ranking exclusion](https://www.freefiremania.com.br/news/free-fire-excludes-emulator-and-shared-accounts-from-ranking.html),
[Possible emulator block](https://freefirejornal.com/en/urgent-garena-may-have-blocked-emulators-in-free-fire-and-free-fire-max/).
