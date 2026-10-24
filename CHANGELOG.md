# Changelog

KnowCode follows [Semantic Versioning](https://semver.org/) and stays on 0.x while its surface settles.

## Unreleased

- Indexing several repos at once no longer spends most of its time in the kernel. SQLite was built with one page
  cache and one allocation counter for the whole process, each behind a global lock, and every connection's page
  fetches and allocations took them. Each connection now has its own page cache, and allocations aren't counted. On
  a workspace of 111 public repos, `index --workspace` used 114 s of user and 355 s of system CPU and 1,236 G
  instructions before, and 75 s user, 36 s system and 949 G instructions after (one run each on a loaded machine).
  The server no longer sets SQLite's 4 MiB process-wide soft memory limit: it needs that counting, and each
  connection's page cache is still bounded by its own size.
- `index` walks the history for the areas beside the file ranking instead of after it. The walk is a `git log` child
  that needs nothing from the ranking; on a repo with a long history it ran alone for most of a cold `index`. On a
  public TypeScript password manager client (8,177 files), a cold `index` took 19.4 to 20.7 s instead of 26.9 to
  34.6 s (three runs each on a loaded machine, so indicative only), for the same instructions (207 G) and CPU time.
  The areas are byte-identical. The background, which ranks on one thread, still walks after ranking.
- `coupled`'s `static_link` and `tests`' reverse walk now run on the symbol graph that `trace`, `impact`, `path` and
  `flow` use, with the same gate, and the old in-repo walker is gone. Their answers don't change: on the 60 most-changed
  source files of each of four public repos with history, 240 `coupled` and 240 `tests` answers are the same before
  and after.
- `context`'s `touches_tables` gives the same answer however the repo was indexed. When a method reaches a table
  several ways at one hop, the row now names the first by file and line, not the first the index happened to store.
  A walk cut at its limit visits symbols in file and line order, so it stops at the same place every time. Before,
  indexing with several parser threads (the default) could change the row's `via` and `site`, and which tables a cut
  walk listed. On the public study's password manager server, all 884 controller methods now answer the same with
  one parser thread and with the default. Against 0.2's single-thread answers, 94 of them name another `via` for
  245 rows, each one the same table at the same hop, and no table is added or dropped.

- The file ranking (the reference graph behind `overview`'s map and entry points, `hotspots`' coupling, `areas`, and
  the order of long `refs` and `impact` lists) costs a seventh to a fortieth of what it did, with the same rankings.
  Resolving every name in turn read again, for each name, what doesn't depend on it: a C# repo's project files and
  `using`s, a TypeScript repo's whole file list, imports and Angular root providers. That is now read once per graph,
  and two queries finding an Angular provider's root injector and `InjectionToken`s start from the few sites by name
  instead of every import or top-level symbol. Building the graph from a cold index, in instructions on one thread:
  a small Python web framework 0.80 to 0.48 G, a .NET media server 156 to 22 G, a .NET password manager's server
  664 to 93 G and its TypeScript clients 3,401 to 81 G. A name's references resolve a few thousand at a time,
  and no longer sorted: the server's peak resident memory building it went from 1,177 MiB to 182 MiB. Several
  threads may share the work (two at most for now: on a loaded machine more cost more instructions than they
  saved), each within the memory a repo's indexing is planned for. The graphs, ranks and areas are byte-identical
  before and after on those four repos.
- `knowcode index` builds the file ranking and the areas when they are missing, not only when an earlier query had
  computed them, and a server builds them in the background, at background priority and one repo at a time, for each
  repo it indexes for the first time and each indexed repo it finds without them: the first `overview`, `hotspots`
  or `areas` call no longer computes them. A cold `index` costs that much more: on the four repos above, 1.8 to 2.4,
  73 to 100, 514 to 624 and 116 to 209 G instructions, with peak resident memory up to 626 MiB (the clients). A
  query needing the graph while the background builds it waits for that build rather than building it again. One
  that only orders a long list by it (`refs`, `impact`, `overview`'s entry points) waits until its freshness
  deadline, then answers in index order and says so (`deadline`).
- A cold `knowcode index` takes less memory at its peak, with the same index, rankings and answers. Most of the peak
  was the history walk behind `hotspots` and `areas`, a `git log` that mapped the repo's pack files and cached delta
  bases with git's defaults: it now runs (as do the ownership walks) within 64 MiB of mapped pack and a 32 MiB delta
  cache. The content index's trigram pairs, freed before the file ranking is built, stayed counted against the
  process; they are now mapped and unmapped on their own. Peak resident memory of a cold index on the four repos
  above, over two runs each: about the same on the smallest, a tenth less on the media server, a fifth less on the
  password manager's server and less than half on its clients. The walk alone on the clients went from 631 to 121
  MiB, for 9% more of git's instructions.

- `context`'s DI flag `conflicting` counts plain registrations only. A `TryAdd*` registration stands back when the
  service has one already, `TryAddEnumerable` adds one of several implementations meant to be resolved together, and
  `Replace` takes the place of the one before: none of them is a second implementation competing for the service, and
  each was counted as one. On the public study's workspace 7 services lose a false `conflicting` flag (a password
  manager's push engines, event writers and integration services registered in exclusive branches through `TryAdd*`
  or `TryAddEnumerable`); no other answer of `context` on its 1,565 DI services changes. Each index is rebuilt once on
  the first query after updating (index schema 21); endpoint registrations whose routes can't be placed are now kept
  with the other constructs KnowCode doesn't read, and answers on them are unchanged.
- `kc_search`'s first page shown whole stops after the last hit in code. Since 0.2.0 a first page at the default limit
  showed every hit when there were at most 60 and they fit; hits in tests, comments and prose past the first 20 now
  stay a page away, and `rest` says that is what they are (all of them still show when a search finds nothing else).
  In the public study's sessions agents used 47 rows past the first 20 of such answers, every one a line of code
  outside tests; none of the 55 test or 30 comment rows there. On the 497 distinct `kc_search` calls of
  the public-v1 runs and a held-out A/B run, answers are 1.0% smaller (901,956 to 893,090 bytes): 13.8% of what
  showing every hit added (64,455 bytes over a 20-row page), with every row agents used still shown.

- A generated API client's operation links only to actions in the servers its repo is known to call: the repo itself,
  a repo it depends on, the one repo serving another of its requests or a host its settings name, or the one repo a
  tagged operation of its links to. Before, an untagged operation (`this.apiClient.getUser()`) linked to a same-named
  action in any indexed repo. With no server known, the links stay, `ambiguous`, with the new assumption
  `server_unverified`. On a public workspace of a media server, a password manager and their web clients, 16 of the
  media client's operations linked into the password manager's server; none do now, and its 349 links into its own
  server are unchanged.
- Microsoft DI service descriptors are registrations: `services.Add(new ServiceDescriptor(typeof(IFoo), typeof(Foo),
  ServiceLifetime.Scoped))`, `ServiceDescriptor.Singleton/Scoped/Transient(…)` and `ServiceDescriptor.Describe(…)`
  handed to `Add`, `TryAdd`, `TryAddEnumerable`, `Replace` or `Insert`, with the lifetime they state and the
  implementation, factory or instance they name. A keyed descriptor is listed as unread. On the public study's
  workspace this adds the 23 production registrations written this way (authorization handlers, push engines,
  hosted services, an OpenAPI provider replaced through `Replace`); none were read before.
- `kc_search` checks repos for freshness on worker threads; those workers now run as the call: a cancelled call stops
  them, the call's freshness wait applies there, and what they note (gone checkouts, repos left out) reaches the
  answer. The repos an answer read from a cached summary, and the indexes it used, are kept per query instead of per
  thread.
- A cited site carries its statement, not just its line. When the call, request or reference a row cites is part of a
  statement over several lines, `cite` quotes the rest of it, at most 2 lines each side: the arguments a call's line
  opens, the builder call a `.SingleInstance()` line continues, the condition of a `? :` branch, the `case` label a
  line runs under. `at` is the range quoted; `cut` says where a longer statement starts or that it goes on. This
  covers `kc_refs` items, `kc_context` callers (and callees its `source` doesn't quote), `kc_impact` rows,
  `kc_trace` nodes and `kc_path` and `kc_flow` steps; text trees mark the statement's other lines with `-`. A line
  that stands alone, or a member of an initializer in braces, still quotes one line. Over MCP the size cap drops
  statement lines, from the last row up, before it cuts a row. On the 67 distinct `kc_refs`, `kc_context`,
  `kc_impact` and `kc_trace` calls of the public study's runs, answers grow 1.7% (295,622 to 300,592 bytes) and show
  the same 720 rows.
- `index --phases` reports what each indexing phase cost: opening the index, discovery, stat, parse, writing rows,
  committing, SCIP, the text content index (read and write) and derived data (file ranking and areas, which it computes
  so they are counted), each with wall and CPU time, the CPU time of child processes (git), and on macOS the
  instructions retired and the phase's share of them. `scripts/index-phases.py` runs it cold and warm over repos and
  prints the medians.
- Repos under the workspace roots are indexed by a running server without an `index` command or a query touching
  them ([#40](https://github.com/bubstackdev/knowcode/issues/40)). The server checks the roots when it starts and as
  workspace queries arrive, reads a new repo's AI privacy file first (a repo blocked whole is never indexed), and
  indexes new repos in the background, smallest first, never ahead of a query's own indexing, and only while the cache
  is below nine tenths of `cache.max_size`. Until then, a workspace-wide answer says "N repositories found, not yet
  indexed" in one `incomplete` entry naming them, instead of one entry per repo or none, and `status` marks each
  `background: "queued"` (or `cache_full`). A repo removed from a root drops out of the workspace.
- Indexing adapts to the machine ([#41](https://github.com/bubstackdev/knowcode/issues/41)). `index` of several repos
  (`--workspace`, or several paths) indexes them at once, as many as half the memory and every core but one allow,
  with at most 8 parser threads a repo; `--jobs` sets how many. Background indexing of the workspace's new repos reads
  the cores, memory, load, memory pressure and power source every few seconds: idle and on power it indexes a repo
  per four cores, under load, on battery or under memory pressure one at a time on one thread, and under critical
  memory pressure it starts none. Queries still come first.
- Workspace-wide MCP answers (`workspace: true`) read the repos with a full index and name the rest in their "not
  yet indexed" entry, whatever form the answer takes, instead of opening every repo of the workspace: a `kc_search`
  over a workspace of new repos indexed each one in its search child, which has no freshness budget, until the child's
  deadline cut the answer to nothing. When no repo has a full index yet, the answer reads the smallest.
- `scripts/workspace-gate.py` is the workspace gate: it clones a pinned public workspace (`scripts/workspace-gate/repos.txt`,
  111 repos), runs `serve` with no `index` command, and checks that every repo becomes indexed and current, that a
  repo added under the root is noticed and indexed, and that workspace answers name every repo not indexed yet.

## 0.2.0 (2026-10-22)

0.2 set out to make answers that agents use without re-checking them, at a lower cost than reading files, and to
show it in a [public, pre-registered study](eval/public-v1/results.md). On the released build the study met two of
its four criteria: answers were as accurate as the plain-tools arm (F1 and recall equal), and agents used KnowCode
without being told to in 24 of 24 sessions. It missed the other two: agents still re-checked about half of
KnowCode's answers (the bar was a halving of the baseline rate), and sessions used about as many tokens as plain
ones, not fewer. The results page reports every run and failure.

- `serve` advertises the `core` tools by default: `kc_find`, `kc_locate`, `kc_context`, `kc_refs`, `kc_impact`,
  `kc_search`, `kc_more` and `kc_help`. A client that doesn't defer tools loads every advertised schema and the
  server's instructions on every turn; for the default that is now 3,462 characters, down from 7,186 (all 25 tools).
  `serve --tools all` (or `KNOWCODE_TOOLS=all`) still advertises every tool, and a comma list adds tools by name
  (`core,kc_trace`). `kc_trace` left the core set (kc_impact answers callers by hop). The core tools are the ones
  marked always loaded (`anthropic/alwaysLoad`), so under `all` Claude Code loads the same eight up front and defers
  the rest. The Claude Code plugin's server sets `KNOWCODE_TOOLS=all` unless the user set it, since its skills use
  the other tools. The server instructions map questions to the core tools again (where X is defined, every use of X, what X
  is, where a concept lives, text, what calls X or breaks if it changes), leading with "use it instead of grep and
  reading files", and say every answer states `complete`.
- The size cap cuts the lists an answer is about last: implementations and conformers, DI registrations, HTTP, bus and
  SQL links. Callers, callees and members go down to none, the source is shortened and those lists' rows drop their
  signature and doc first; links are text rows with what they share said once. On a public DI library, `kc_context` of
  an interface cut 7 of its 11 conformers to keep callers; it now shows all 11. On a public media server, a controller
  action's context showed 3 of its 39 HTTP callers; it now shows all 39 in the same 6,000 characters.
- An ASP.NET controller marked `[Route("")]` serves its actions at the root: the empty prefix replaces a base
  controller's `[Route("[controller]")]`, as ASP.NET takes it, where KnowCode placed them under the base's prefix
  (`Widgets/Items/{id}` for `Items/{id}`). On a public media server 23 controllers do this; 16 requests from its web
  client and 16 from its own tests now link to their actions, and none lost a link.
- A `kc_search` page cut short names the other files with a match (`other_files`, by directory, with counts, up to 60
  files), so the whole set is seen without paging or searching again.
- `complete` speaks for what was asked. A list on the side of it that a limit or `max_chars` cut (a symbol's callers,
  callees and members, the tables its calls reach, one reference's candidate targets, its source, cites past the first
  few without their line) is a new `more` entry, with the call for the rest, and the answer stays `complete`; the lists
  an answer is about (its items, implementations, DI registrations, links, a walk's nodes) are still `incomplete` when
  cut. `kc_find` cut only past every match with the name asked for, and `kc_search` cut with every file with a match
  named, say what the rest are (`rest`) and are `complete`. In a public A/B study most re-checks after a KnowCode answer
  followed `complete: false` from a cut side list; on the 14 such calls of one arm, all 14 now answer `complete: true`
  (77,966 to 81,170 characters).
- `kc_search` shows every hit on a first page at the default limit when there are at most 60 and they fit `max_chars`.
  On the study's 64 `kc_search` calls, the 29 that were cut are now `complete`, 17 of them showing every hit; answers
  grow from 178,576 to 206,676 characters, 26,934 of it on those 17 (the 35 uncut answers are unchanged).
- A container registering itself is a DI registration: a registration built from its parts (`new
  ComponentRegistration(…)` or `base(…)` in a class deriving it) registers each `TypedService(typeof(X))` it names as
  its activator's type. On a public DI library, `kc_context` of its resolving interface now lists the registration.
- A `file:line` or `path#name` written after its repo's name (`shop/src/a.cs:12`, the way questions and cross-repo
  answers name places) resolves in `kc_context`, `kc_impact`, `kc_trace` and `kc_refs` (which answers for the
  definition there); `kc_search` takes `**/shop/src/**` as it takes `shop/src/**`. A place naming no definition, and a
  search filter matching no file, are refused saying what didn't resolve (no such file, or the definitions nearest the
  line), where they answered `complete: true` with no items.
- MCP answers say `complete: true` again when they are complete (0.2.0's leaner answers said it only when false). A
  text tree ends its first line with `; complete`.
- `find`'s `declared` speaks for the workspace: "the only declaration of X in the workspace (6 repos) is in autofac",
  or each repo when several declare it, whichever repo the lookup was scoped to (the other repos are ruled out by their
  summaries, or opened when they may declare or name it; a repo without a full index is never indexed for it and is
  said not checked, and other checkouts of the repo asked are left out). Its reference count covers every repo. When files KnowCode
  doesn't read as code name the symbol too, the count is "at least N" and those lines are named.
- JavaScript under an app's own `src/.../plugins/` is indexed. It was taken for vendored jQuery plugins and left out,
  so uses through named imports and namespace imports (`import * as m`, `m.f()`) there were missing from `refs` and
  `declared` counted too few (2 references instead of 8 on a public web client). A `plugins/` directory outside `src`,
  or under a web root within it, is still vendored.
- Notes about files and constructs KnowCode doesn't read are checked before they are made. Razor files (`.razor`,
  `.cshtml`) are searched as text for the symbol: `refs` and `impact` name the lines that name it, and when none does,
  `refs` says so in `checked` and stays `complete`, instead of a note that the files exist. `refs` also names lines in
  other files not read as code (vendored JavaScript) that name it. An assembly scan bears on a type only when its
  argument's assembly (`typeof(T).Assembly`, `GetExecutingAssembly()`) is a project holding the type or an
  implementation of it; a scan of an assembly the code doesn't name is an `unsupported` entry only for a type the
  container knows (registered or injected), and otherwise a clause of its "DI registrations: none found" line.
- A repo indexed under `GIT_CONFIG_GLOBAL=/dev/null`, as sandboxes and CI set it, is no longer said to have "changed
  since its last index, and indexing is queued or running" on every answer. The freshness check stamped the global
  git config by its mtime, and the mtime of `/dev/null` moves on every write to it anywhere on the machine; a device
  or pipe is now stamped by its kind alone.
- DI constructs KnowCode doesn't read that can't register the type asked about are left out of the `unsupported`
  entry and counted in its "DI registrations: none found" line: those in test code (a test's own container), for a
  type outside it, and, for an interface, an assembly scan exposing only other services (`.As<IModule>()`). On a
  public DI library, `context IComponentContext` listed 13 assembly scans (12 in tests, 1 registering only modules);
  it now lists none and says why. Razor files are checked as text for any number of names up to 200, in batches,
  where more than 20 used to give the note that the files exist; a Razor file search can't read is named on its own.
  `checked` says "none of 59 Razor files (.razor, .cshtml), checked as text, names X".
- Lower memory in search and indexing, with answers unchanged. Trigram lists are intersected one at a time, in place, starting from the smallest; indexing workers allocate the trigram set only for files they index; a file read reserves its own size instead of growing twice. Path globs can no longer take exponential time: a bounded matcher takes over after a fixed amount of backtracking. The revision and manifest-version caches keep at most 1,024 entries each, so a long-running server no longer grows with every checkout state it has seen.
- Leaner MCP answers and tool list. A clean, current answer no longer lists its repos: `repos` and `provenance` are
  left out unless a repo is dirty or was answered from its last sync, or something is `stale`, `failed` or
  `missing_repo`. `complete` is said only when it is `false` (no `incomplete` means complete), and false flags, nulls,
  `offset: 0` and a search's `stats` are left out. `stale` is one `incomplete` entry naming repos by name, not path.
  `kc_search` and `kc_grep_ast` hits, and `kc_more` pages of them, are text rows (`repo/file:line kind symbol |
  text`) instead of objects; docs/answers.md has the format. On a three-repo sample, search answers are 40-60%
  smaller. The tool schemas and server instructions under profile `all` are 7,186 characters, down from 10,641:
  shorter descriptions and parameter lines, fewer advertised parameters (the rest are still accepted and in
  `kc_help <tool>`), and instructions that no longer repeat the descriptions. The instructions and the `kc_refs` and
  `kc_find` descriptions now send "what calls X" and "what breaks if X changes" to `kc_impact` (or `kc_trace`, depth
  2) with a `file:line` selector. The CLI's output and `--json` are unchanged.
- Answers whole enough to stop on. `find` on an exact name adds `declared`: one line per repo saying it has every
  declaration of the name there, with a C# `partial` type's parts (file:line each), a type's constructors (a record's
  primary one too), a method's overloads, and how many references in code name it (the other mentions are uses, for
  `refs`). `refs` groups references by caller (code before tests), marks rows in test files `test`, tallies callers
  and sites in `callers`, numbers the overloads the references resolve to (`overloads`, with signatures and the sites
  reaching each) and says which one each row calls (`overload`, in place of its `targets`). For an interface or base
  class, `refs` adds `hierarchy` (implementers or subclasses, those reaching it through another type, derived
  interfaces, DI registrations of it and of them) and `context` adds `conformers_indirect` and `di_implementers`; both
  say each empty list as "none found" with what was searched (`none_found`). A `context` on a partial type lists its
  `parts` instead of calling them other definitions, and lists constructors first among members.
- HTTP routes across repos find more of their callers. A controller with no `[Route]` of its own takes the nearest
  one up its base classes, in any file of the repo, with `[controller]` its own name, as ASP.NET does; `[Area]`,
  `[ActionName]`, `[NonAction]`, `[RoutePrefix]` and a method's lone `[Route]` (every verb) are read. Conventional
  routes with a literal template (`MapControllerRoute`, `MapDefaultControllerRoute`, `MapRoute`, `MapHttpRoute`) now
  place the actions they serve instead of making answers incomplete. Requests are read from `fetch`, axios (and
  instances of it), jQuery's `$.ajax`, `$.get` and `$.getJSON`, superagent, and wrappers named for HTTP or API
  clients, with URLs built from a base and a path (`getUrl('Items')`, `new URL(path, base)`, C#
  `new Uri(base, path)`) and RestSharp's `RestRequest`. A call to a generated API client
  (`getItemsApi(api).getItems()`, `new OrdersClient(base).list()`, `this.ordersApi.getOrder(id)`) links to the
  action of that name, under the controller named like the client, assuming `operation_name`. Each extractor's
  version is bumped, so C# and TypeScript files are read again on the next query.
- An HTTP fact with no links says what is left to check (`note`): for an action, the repos making requests that
  were searched and how, the generated clients seen, the requests whose URL is known only at run time (with where),
  and the requests nearest by path (another prefix); for a request, the repos serving routes and the nearest actions.
  `routes` lists, per unlinked request, the nearest actions only.
- `knowcode fetch-deps --into DIR` clones the git repos a repo depends on (manifest dependencies written as git
  URLs, Terraform git module sources, GitLab CI includes of other projects, and repos whose checkout was deleted),
  shallow, one directory each, so cross-repo answers can reach them. `--dry-run` prints the plan. DIR must be outside
  every repo; only https and ssh remotes are cloned; a repo whose AI privacy file blocks it whole is skipped unless
  `--include-blocked`. `--add-root` adds DIR as a workspace root. CLI only: the MCP server never clones.
- `knowcode history deepen` fetches more history into a shallow clone (`--by N`, 1000 by default, or `--since DATE`)
  and reports how deep it is before and after, and whether `hotspots`, `coupled`, `ownership` and `reviewers` now
  have all the history they read.
- Each index is rebuilt once on the first query after updating (index schema 20): constructs KnowCode doesn't read
  are now kept in a table of their own. Answers are unchanged.
- Smaller MCP answers by default. `kc_refs` lists 20 references (was 25), `kc_trace` 25 nodes (was 40), `kc_impact`
  30 dependents (was 60), `kc_flow` 40 steps (was 60), `kc_unused` 25 findings (was 50). The `kc_context` budget is
  1200 tokens (was 2000), capped at 6000 characters, and the `kc_overview` map is 400 tokens (was 800). Every cut says
  what it left out, and the rest comes through `kc_more` or the parameter it names. MCP rows are leaner. A
  reference's `from` is the enclosing symbol's qualified name, with no column. What all rows of a reference list
  share (`targets`, `from`, `name`) is said once beside the list. Test files drop their ranking `signals`. Tree use
  sites in the symbol's own file are said by line. The CLI's output and `--json` are unchanged.
  `scripts/mcp_tokens.py` measures answer sizes, with `--count-tokens` and `--compare`.
- Smaller MCP tool schemas: each tool's schema lists only its common parameters, with short lines. `repo` and
  `max_chars`, which the server instructions state once, and the rarer parameters (`as_of`, `since`, `order`,
  `format`, `kinds`, `evidence`, `edges`, `cite`, …) are still accepted and documented in `kc_help <tool>`. Profile
  `all` costs about 4.5k tokens per turn for a client that loads every schema (was about 8.6k), and the four
  always-loaded tools about 0.8k (was about 1.8k); see eval/schema-cost.md.
- The MCP server's instructions, the descriptions of `kc_find`, `kc_locate`, `kc_refs`, `kc_context`, `kc_trace`,
  `kc_impact` and `kc_search`, and the plugin's skill descriptions now lead with the questions they answer ("who calls
  or uses", "where is", "what breaks if"). None is longer than before.
- The clean-machine acceptance report adds `knowcode-use.json`: KnowCode calls per case and session, by tool.
- Cited source: every row that gives a location (`trace` nodes, `impact` rows, `path` and `flow` steps, `refs` items,
  a `context`'s callers and callees) carries `cite`, the source lines it rests on, read from the working tree when
  the answer is built: an edge's site, else the line naming a declaration. By default (`cite: brief`) the first 12
  rows quote one line each, up to 120 characters, and the rest give where it is, with a `capped` entry saying so.
  `--cite lines` (MCP `cite: "lines"`) quotes every row, a declaration from its first line through its name (at most
  3), each line up to 160 characters; `cut` says what a cap left out. `--cite none` leaves them out. Text trees print
  the lines under each row. A `context`'s callers quote their line in `cite` instead of `code`, which remains under
  `cite: none`.
- Cites are checked at query time: each cited file is compared with the index the answer read, and `check` says
  `verified`, or names the drift: `changed` (the line still names what it cites), `moved` (quoted where it is now,
  with `was`), `gone`, or `unchecked`. Every cite is checked, past the brief quote cap too. Any drift makes the answer
  `stale`, naming the cites. `--cite check` (MCP `cite: "check"`) gives the check without the lines. A `context`'s own
  symbol carries a check-only `cite`.
- Walks that finish: every place a walk stops short gives the exact call going on from it. Each `frontier` stop of
  `trace` and `impact` has `next` (`kc_impact {"depth":3,"repo":"web","symbol":"OrderService.place"}`), and the
  `incomplete` hint ends with the first one. A `flow` step at the depth limit that leads on has `stop` `{reason,
  beyond, next}`, and the flow is incomplete (`depth`); a flow cut by its step or repo limit, and a path search cut by
  its node or repo budget, are `capped` with the call raising the limits; a path search finding nothing within its
  hops has `next`, the call looking further.
- `trace` and `impact` now walk the same symbol graph as `path` and `flow`, through the same gate. One visible change:
  a hop across a link matched by name, from code found `exact`, is now `unique_name` in `trace` and `impact` too, as
  `path` and `flow` always labelled it. That covers an SQL object matched by its name (evidence `sql_name`) and one of
  several HTTP handlers or SQL schemas preferred by a heuristic (`dominant`, evidence `name_dominant`). The evidence is
  as before; the hop now counts in `impact`'s `low_confidence`. Under the default evidence policy it is still followed;
  under `confirmed` it is still left out.

- When a name has definitions in code and in tests (a test helper named like the function it tests), the code's
  answers first, whatever the kinds: `context`, `trace`, `impact` and the other symbol lookups used to rank a test
  file's function above a source file's constant. The test's definition is listed among the alternatives.
- Razor files (`.razor`, `.cshtml`) aren't read. A C# `refs` or `impact` answer from a repo holding any is now
  incomplete with reason `unsupported`, naming one and saying their markup's uses aren't listed, with a `kc_search`
  hint, instead of looking complete.
- The pre-registered public agent study measured this release's candidate (commit 00aae5b): it missed 2 of its 4
  criteria (re-check rate, tokens) and met 2 (answer F1 and recall, unprompted use). See
  [eval/public-v1/results.md](eval/public-v1/results.md).
- `search` and `grep-ast` (`kc_search`, `kc_grep_ast`) take a `paths` glob written after a repo's name
  (`web/**`, `web/src/**`): it searches that repo only, relative to it. Such a glob used to match nothing, and the
  answer said "no line matches" in every repo. An absolute path searches the repo it is in. A `lang` may be an
  extension (`ts`, `cs`, `py`). When the filter matches no file, the note says `paths matched 0 files (…)` (or
  `lang matched 0 files`), naming the repos, instead of a no-match note for lines that were never read.
- `at` (`kc_symbol_at`) finds the file's repo across the workspace: a path after the repo's name (`web/src/a.ts`),
  one relative to a workspace root, a repo-relative path held by exactly one repo, an absolute path inside a repo, and
  an absolute path outside any repo holding a repo's name (a copy of the tree, a stack frame from another machine).
  It used to refuse any relative path in a workspace ("pass --repo") and any absolute path outside a repo ("not
  inside a git repo"). A path in several repos names them.
- `index` brings derived data earlier queries computed (areas, the file ranking) up to the new index. An MCP call on
  a current index no longer waits for that work: it was scheduled on the session's first call, which then could run
  out of its budget and say "indexing is queued or running" right after `index`. The note is now only for repos
  that changed since their index, and only where it bears on the answer (the repo asked about, one never indexed, or
  one the answer's items come from). It names the repo, not its local path.
- C#: an extension method is found by the static class declaring it (`RegistrationExtensions.RegisterDecorator`) as
  well as by the type it extends: `find`, `context`, `trace`, `impact` and the other lookups, and `refs`, which
  answered nothing for it. A constructor initializer on the line after the colon (`… int x) :` then `base(x)`) is
  placed at `base`/`this`, not a line early. C# files are reparsed once.
- `search` hits carry what an agent opened the file for after one matched line. The first code hit inside a method
  or function names its parameters (`in EventSink.Emit(eventName, payload)`), once per member on a
  page. Without `context`, a hit in a statement that spans lines gets those lines, at most two each side: the call a
  `.fetch(…)` line continues, the `.then(…)` continuing an `ordersApi(client)` line, the `case` label a line
  runs under. A line that stands alone gets nothing. On the 46 `kc_search` calls of a public A/B study whose answers
  were re-checked, answers grow 6.2% (104,001 to 110,450 characters): 33 of 560 rows gain statement lines, 204 gain
  parameter names.
- `find` quotes the line naming each declaration of the name asked for in `cite`, read from the file now and checked
  against the index (`check: verified`), past doc comments and attributes: `[HttpGet("{id}")]` is not the line quoted
  for an action. One line per item, the first 12 such items.

## 0.1.1 (2026-10-20)

- A workspace-wide answer no longer fails because one repo can't be read (a checkout deleted while the answer was
  worked out, a failing `git` call): it answers with the other repos and names the one left out as `missing_repo` or
  `failed`.

- Updating refreshes the Claude Code plugin: after `self-update`, and when `serve` starts with an older bundle,
  KnowCode rewrites the bundle and runs Claude Code's plugin update. If that can't finish, it prints the
  `knowcode setup claude` command that does. `setup claude` also moves an installed plugin to the current version.
- The Claude Code hook script exits without starting `knowcode` for Bash commands that can't be code searches.

- `status` and `kc_status` always show the AI privacy policy they apply, indexed or not: the file's state, the file
  name looked for, the global level, `block_at` and how many paths are blocked. The CLI text gives it one line.
- `uninstall` no longer asks Claude Code to keep the plugin's data directory, where KnowCode keeps nothing. Claude
  Code's orphaned cache copy of the plugin is left to Claude Code, for sessions still running.
- Tests: the shared `commit` test helper reports git's output when a fixture commit fails.

- Homebrew is back: `brew install bubstackdev/tap/knowcode`. For a Homebrew installation `self-update` refuses with
  "Homebrew manages this install; run `brew upgrade bubstackdev/tap/knowcode`", and the update notice and
  `self-update --check` name that command. After `brew upgrade`, the next `serve` start refreshes the Claude Code
  plugin.

### Release

- Releases publish a Homebrew formula to `bubstackdev/homebrew-tap` again. The publish job commits as "KnowCode
  release bot" with a noreply address, has read-only `GITHUB_TOKEN` permissions, and is the only job given the
  `HOMEBREW_TAP_TOKEN` secret, which reaches only its push step, through `env:`. On a tag push, CI first checks that
  the tap is public and that the token can push to it; pull requests and pushes to `main` need no secret.
- Release binaries no longer contain build-machine paths. The C code in tree-sitter and its grammars now gets the same
  path remapping as Rust code, and each release build fails if a binary contains the builder's home or checkout path.

### Release testing

- Clean-machine acceptance is one script in the repo, `scripts/acceptance/clean-machine.sh`, for macOS and Linux. It
  never starts an interactive shell, covers the installer adding `~/.local/bin` to PATH, compares client settings
  entry by entry from a snapshot taken right before uninstall, records two client sessions per case from the
  client's transcripts (recorded, not gated), and checks that a new shell starts cleanly after the removal steps.

## 0.1.0 (2026-10-19)

The first public release, for macOS on Apple silicon and Linux on x86_64 and arm64.

### Answers

- One answer contract for every command and MCP tool: `complete`, `incomplete` with closed-set reasons (`capped`,
  `budget`, `deadline`, `stale`, `failed`, `missing_repo`, `unsupported`, `policy`, `withheld`), `sampled`,
  `provenance` for every repo consulted, and cursors that continue a cut list exactly once (`knowcode more`,
  `kc_more`).
- Every edge carries a confidence (`exact`, `unique_name`, `ambiguous`, `external_likely`, `unresolved`) and evidence:
  how the target was found and what that did not check. An evidence policy (`--evidence`) chooses which edges a walk
  follows.

### Questions it answers

- Definitions by name (`find`), files for a concept (`locate`), symbols at a line (`at`), ranked text and regex search
  (`search`), and structural search with ast-grep patterns (`grep-ast`).
- One bundle per symbol (`context`), every reference (`refs`), call trees (`trace`), paths between symbols (`path`),
  and end-to-end flows from an entry point (`flow`), across repos where HTTP routes, the message bus, SQL,
  dependency injection, Terraform modules or Ansible roles link them.
- Before a change: `impact` by symbol, file or diff, with dependents by proximity and co-changed files; `tests` to
  run; `coupled` files; `reviewers`; dead-code candidates (`unused`) and rename previews (`rename`) for C# and
  TypeScript.
- History: `hotspots` with code-health biomarkers, `ownership`, and functional `areas`.
- How a repo is deployed, from its GitLab CI pipeline through shared templates to Ansible, Terraform, cloud CLIs,
  containers and releases (`context <repo name>`), with every include accounted for and every list continuing by
  cursor.
- Constructs an extractor recognises but doesn't read (DI assembly scans, run-time SQL table names, publishes of
  unknown message types, Jinja-named includes, Terraform `moved` blocks, unread CI include and Angular lazy-route
  forms) make the answers drawing on them incomplete (`unsupported`), naming each.
- Orientation: `overview`, `deps`, `routes`, `tables`, `events`, `docs`, `status`.
- Notes anchored to symbols, kept in the user data directory, with export and import.

### Languages

- C#, TypeScript, TSX and JavaScript, Swift, Python, Rust, ColdFusion, SQL, Terraform, Ansible and GitLab CI, with
  manifests from SwiftPM, XcodeGen, Cargo, MSBuild and npm. Angular, ASP.NET, Entity Framework, MassTransit,
  NServiceBus and common DI containers are modelled. Precise references are read from a SCIP index when one is
  present. See [languages](docs/languages.md) for what is and isn't covered.
- A code base's own conventions (message-type attributes, consumer bases and publish helpers, gateway calls and
  request contracts, DI scope values, CI and Ansible names) are configured under `[conventions.*]` in `config.toml`,
  so its messages and requests link as the libraries' do.

### Running it

- `knowcode setup claude`, `codex` or `cursor` connects a client; the Claude Code plugin adds a Grep/Glob/Bash hook and
  four workflow skills. `knowcode uninstall` removes what setup recorded, checks that the client no longer lists
  it, and ends with the exact steps to remove a shell-installer installation.
- In Claude Code, `kc_context`, `kc_refs`, `kc_impact` and `kc_locate` load up front (`anthropic/alwaysLoad`, about
  1k tokens); the other tools stay behind tool search.
- The first query indexes automatically. MCP queries wait about two seconds and otherwise answer from the last
  snapshot, marked stale, while a low-priority background worker finishes.
- Indexes live in the user cache directory, bounded by `cache.max_size` (2 GiB by default), with deleted checkouts
  pruned automatically. Nothing is written into indexed repos.
- One `config.toml` for settings, `knowcode config` to edit it, and workspace roots for multi-repo answers.
- Repos can block content from AI tools with a privacy file (`.knowcode-privacy.yml`, or the name and keys
  `[privacy]` configures); blocked content never reaches an answer or the cache.
- No telemetry. Indexing and queries make no network calls. A daily, notify-only update check can be turned off.
- Shell installer built with cargo-dist; `knowcode self-update` for shell installs. Homebrew follows in 0.1.1.
