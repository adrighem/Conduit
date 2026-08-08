# Changelog

## [0.4.0](https://github.com/adrighem/Conduit/compare/v0.3.0...v0.4.0) (2026-08-08)


### Features

* **assets:** serve bounded timeline cache entries ([44f1c0f](https://github.com/adrighem/Conduit/commit/44f1c0f6979eba2fe72239e9f95a912c7600becc))
* **composer:** add rich formatting toolbars ([467cc7d](https://github.com/adrighem/Conduit/commit/467cc7d045af65b432d3618f7d6dda5983bcc123))
* **composer:** model Slack rich text ([096cc88](https://github.com/adrighem/Conduit/commit/096cc888a4404d7cc82ed037e89a3e1c847ec38f))
* **composer:** persist and send rich drafts ([62a0d7a](https://github.com/adrighem/Conduit/commit/62a0d7a27fa9afd7739cf2b142d36e2ea19e12fc))
* **composer:** send staged media in one batch ([2bfa87e](https://github.com/adrighem/Conduit/commit/2bfa87e9f8e52d75974ecb07d2bd93d8000f7771))
* **composer:** stage attachment previews ([e9ed127](https://github.com/adrighem/Conduit/commit/e9ed1276e3ac4ae3c916c4ea9adfdfa903e5e397))
* **emoji:** add bounded picker bridge protocol ([d9382e8](https://github.com/adrighem/Conduit/commit/d9382e8cc2cbcd6ae14d3c9fe0e0d24ddd689bdc)), closes [#10](https://github.com/adrighem/Conduit/issues/10)
* **emoji:** materialize picker results on demand ([c115324](https://github.com/adrighem/Conduit/commit/c11532455d9b4ff64e803f5e04a4eb6d4e50c1ce)), closes [#10](https://github.com/adrighem/Conduit/issues/10)
* **messages:** edit last sent message ([c5c1d08](https://github.com/adrighem/Conduit/commit/c5c1d085fac569529c62c155b809e1e8470dd750))
* **messages:** execute Slack app callback buttons ([acfdc0c](https://github.com/adrighem/Conduit/commit/acfdc0c800d255636ff0c4e8fad7a25a53c1bfa2))
* **metrics:** add pipeline activity counters ([0e3939f](https://github.com/adrighem/Conduit/commit/0e3939f6ea348a903c26a63a44f92a000b23f313)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **navigation:** restore last conversation ([725a59c](https://github.com/adrighem/Conduit/commit/725a59c8ef31e89421738d588ddd5e74be84e847))
* **reactions:** rank four quick responses by usage ([8528d0a](https://github.com/adrighem/Conduit/commit/8528d0ada351f69bd7c99c6584710994598076c8))
* **runtime:** integrate bounded SyncJob scheduler ([e056d48](https://github.com/adrighem/Conduit/commit/e056d4804c06f00902c837a226803714d0ad4927)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **sidebar:** define keyed projection operations ([5cb5167](https://github.com/adrighem/Conduit/commit/5cb5167ad85503fbe9862a041be36a5b80a0c47c)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **sidebar:** migrate to incremental list view ([fdc127c](https://github.com/adrighem/Conduit/commit/fdc127cb8bc78d01c77af2d79af6522d5cdafe38)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **sidebar:** prioritize unread conversations ([028bc3f](https://github.com/adrighem/Conduit/commit/028bc3fc97ae77a56394ee556ea43520802d0e94))
* **slack:** post rich text blocks ([bb868ec](https://github.com/adrighem/Conduit/commit/bb868ec4c1d1657f96f3d531cfc3d29dbdb06847))
* **status:** align emoji picker layout ([47ee7eb](https://github.com/adrighem/Conduit/commit/47ee7ebe12c54258b2eca1d916311b1fc76e9284)), closes [#10](https://github.com/adrighem/Conduit/issues/10)
* **store:** execute coordinator batches atomically ([820aef1](https://github.com/adrighem/Conduit/commit/820aef1c1201ff125d1a666e2adf7d1ee2bc4ffe)), closes [#11](https://github.com/adrighem/Conduit/issues/11)
* **store:** persist message deltas ([0982111](https://github.com/adrighem/Conduit/commit/0982111e6448917e1f6b37ea3babd4e0a46dfb03)), closes [#11](https://github.com/adrighem/Conduit/issues/11)
* **sync:** add scheduler counters ([147f934](https://github.com/adrighem/Conduit/commit/147f934bebcb2a818f1a2077b43d6a8e6223a110)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **sync:** add scheduler lifecycle ([841d550](https://github.com/adrighem/Conduit/commit/841d5501ea2d517976ec62fa201934ddd423cba4)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **sync:** bound scheduler admission ([cded7d7](https://github.com/adrighem/Conduit/commit/cded7d7e8e0480472cb697ed0fbcb9e2380e2857)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **sync:** define bounded job contracts ([a5b6742](https://github.com/adrighem/Conduit/commit/a5b6742867e451ec47ce650c1735202d2d1ff663)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **thread:** create WebView on first open ([9e415cf](https://github.com/adrighem/Conduit/commit/9e415cfab5622709f623a24721cce47d1e45d8db)), closes [#10](https://github.com/adrighem/Conduit/issues/10)
* **timeline:** batch incremental frame updates ([b1f136a](https://github.com/adrighem/Conduit/commit/b1f136a06aaf8c3829819d5c59c2b29e6d849b49))
* **timeline:** define revisioned presenter contract ([9a37fad](https://github.com/adrighem/Conduit/commit/9a37fada7bbc5dac55ee52e79dcf25304ee1372a)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **timeline:** increase unread message visibility threshold to 90% ([df56697](https://github.com/adrighem/Conduit/commit/df56697873bdc38ac82ebfd11086949ad74f588f))
* **ui:** open profiles from DM titles ([8f92c5f](https://github.com/adrighem/Conduit/commit/8f92c5fa284fc65c3680c09a27cdcca149e7b500))
* **workspace:** centralize conversation authority ([0bff772](https://github.com/adrighem/Conduit/commit/0bff772fed534bc78147de0c873ef3cf2d034389)), closes [#11](https://github.com/adrighem/Conduit/issues/11)
* **workspace:** deliver revisioned conversation patches ([0e39acd](https://github.com/adrighem/Conduit/commit/0e39acd8b51fd53928cea66ec5ae0184af56f46a)), closes [#11](https://github.com/adrighem/Conduit/issues/11)
* **workspace:** persist conversation refresh batches ([e451ce0](https://github.com/adrighem/Conduit/commit/e451ce0d2cfc4ddc73583b375e651e9e95c81999)), closes [#11](https://github.com/adrighem/Conduit/issues/11)
* **workspace:** persist prefetched history ([2eb1c6d](https://github.com/adrighem/Conduit/commit/2eb1c6d889ed8a69c0af6215d2042fe8258cb225)), closes [#11](https://github.com/adrighem/Conduit/issues/11)
* **workspace:** unify history and realtime authority ([3c1adf8](https://github.com/adrighem/Conduit/commit/3c1adf8fc17ef40fe5708b028fb3a2257cda3569)), closes [#11](https://github.com/adrighem/Conduit/issues/11)


### Bug Fixes

* **ci:** release view borrow before await ([20fa47f](https://github.com/adrighem/Conduit/commit/20fa47f0bcd0693319b2a5ea15c2e69491695376)), closes [#11](https://github.com/adrighem/Conduit/issues/11)
* **ci:** satisfy pinned clippy ([84b5474](https://github.com/adrighem/Conduit/commit/84b54744063644e9b7150245c9e3d7253e4ace44)), closes [#11](https://github.com/adrighem/Conduit/issues/11)
* **ci:** scope pending-write test lock ([1523d6c](https://github.com/adrighem/Conduit/commit/1523d6c53b24fa003d1cffbcf4e986accbc360b3)), closes [#11](https://github.com/adrighem/Conduit/issues/11)
* **composer:** make formatting controls square ([6ecc627](https://github.com/adrighem/Conduit/commit/6ecc6272a6acc535553bee770b4fe4bb045d20b0))
* **composer:** preserve rich formatting on send ([6921b56](https://github.com/adrighem/Conduit/commit/6921b5633697a63930d5f6118a164da610dfc1e3))
* **composer:** repair build startup and preview layout ([0b7213f](https://github.com/adrighem/Conduit/commit/0b7213f1b5a1031aa45a45040c4482be9631ee01))
* **composer:** use explicit slice iteration ([9094a5f](https://github.com/adrighem/Conduit/commit/9094a5f390cc617287f42c694f33e55ae65c4471))
* **emoji:** render Slack skin-tone modifiers everywhere ([206a712](https://github.com/adrighem/Conduit/commit/206a712b030526bfca3d5b0035ac250b3fd08bdb))
* **emoji:** resolve Slack canonical reaction names ([82c622d](https://github.com/adrighem/Conduit/commit/82c622d2279f96993bb6f68a70665feec937e561))
* **huddles:** bound native media callbacks ([ad2a153](https://github.com/adrighem/Conduit/commit/ad2a153b409e5068f6aa529150e44b80fbd19a61)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **huddles:** harden session teardown ([38bf6c6](https://github.com/adrighem/Conduit/commit/38bf6c6260e3111892531c2ca9564f70d2b8857f)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **messages:** preserve URL preview images ([c3a7eb1](https://github.com/adrighem/Conduit/commit/c3a7eb169e1f8a469ef93a5ab07cbe5c142b9cd3))
* **messages:** render animated Slack GIF shares ([8129099](https://github.com/adrighem/Conduit/commit/8129099f16a05fde767d2c2e4c2a0d1d0788fd90))
* **messages:** render attachment-embedded GIF blocks ([fbe931c](https://github.com/adrighem/Conduit/commit/fbe931cac763a27ab5b4302f52b95d6fe837f79f))
* **runtime:** admit startup follow-up sync jobs ([91303bb](https://github.com/adrighem/Conduit/commit/91303bb8dfb420c0c67f22e3f8def5fc11feb70b)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **runtime:** bound command admission before task creation ([7ecc75c](https://github.com/adrighem/Conduit/commit/7ecc75c4de439591b8a71aa48a2d4b32b439e3e0)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **runtime:** bound huddle actor admission ([321eab8](https://github.com/adrighem/Conduit/commit/321eab853df09e6ee90b15e25f00189bf07322a0)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **runtime:** bound runtime event publication ([7c644ab](https://github.com/adrighem/Conduit/commit/7c644ab2e14f574632d299743258f29d703d6a3d))
* **runtime:** drain realtime work on session shutdown ([65e16fd](https://github.com/adrighem/Conduit/commit/65e16fdb4cf6d84aea97b372ba5c19377a8e35c5)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **runtime:** optimize startup user directory sync ([6590ad0](https://github.com/adrighem/Conduit/commit/6590ad079985dc8ce057b04e6658eb0977767fc2)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **runtime:** schedule startup workspace refresh with Interactive priority ([be3309d](https://github.com/adrighem/Conduit/commit/be3309d6d68580161c6c8e19ab65f30a21fabd46)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **runtime:** use div_ceil for preview bounds ([a99cd59](https://github.com/adrighem/Conduit/commit/a99cd596f73777bdcc8876902009e163f5fbd564)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **status:** animate custom emoji previews ([a4fcab4](https://github.com/adrighem/Conduit/commit/a4fcab4e8c73b3d88b5d68a1bf2351f7b19e26cc)), closes [#10](https://github.com/adrighem/Conduit/issues/10)
* **status:** render selected custom emoji preview ([51522cd](https://github.com/adrighem/Conduit/commit/51522cdb7b610f36c2f1bea232c087d60f829ebf)), closes [#10](https://github.com/adrighem/Conduit/issues/10)
* **status:** satisfy picker type lint ([46e1f95](https://github.com/adrighem/Conduit/commit/46e1f95d78d40de837193ae584fe7a87aee77156)), closes [#10](https://github.com/adrighem/Conduit/issues/10)
* **store:** harden coordinator batch recovery ([c8b31da](https://github.com/adrighem/Conduit/commit/c8b31daf8f688be655666685ac6eb54b3c3898cd)), closes [#11](https://github.com/adrighem/Conduit/issues/11)
* **store:** preserve conversation authority ([640658d](https://github.com/adrighem/Conduit/commit/640658dfc7261001ec9a1cc02a51dee29fba2d71)), closes [#11](https://github.com/adrighem/Conduit/issues/11)
* **store:** satisfy current clippy iterator rules ([0501fef](https://github.com/adrighem/Conduit/commit/0501fef58a70266e47019fe0fb3ef0bf4701143a)), closes [#11](https://github.com/adrighem/Conduit/issues/11)
* **thread:** satisfy strict lazy renderer lint ([014708a](https://github.com/adrighem/Conduit/commit/014708a0426473e9a0d1ba30e7f34459a969db7c)), closes [#10](https://github.com/adrighem/Conduit/issues/10)
* **timeline:** bound cached preview assets ([a102fca](https://github.com/adrighem/Conduit/commit/a102fcaf63f2d5e6873290c43dbb746eb4a95fc6)), closes [#9](https://github.com/adrighem/Conduit/issues/9)
* **unread:** acknowledge only visible activity ([9d9019b](https://github.com/adrighem/Conduit/commit/9d9019b9c219613d9ba91f7ec5af36b35903db80)), closes [#11](https://github.com/adrighem/Conduit/issues/11)
* **window:** dismiss syncing screen immediately when local cache is loaded ([1e021f4](https://github.com/adrighem/Conduit/commit/1e021f4f8671d621846a74b81129042def94ea0c)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **window:** keep new message picker responsive ([d204494](https://github.com/adrighem/Conduit/commit/d2044943654e0d8fbcef79f50e4a295b2ef3a682))
* **window:** resume cached workspace startup ([706c496](https://github.com/adrighem/Conduit/commit/706c496604e6dc28efc39adbefc1c7ec9b00476a)), closes [#12](https://github.com/adrighem/Conduit/issues/12)


### Performance Improvements

* **runtime:** bound realtime persistence ([fcf641c](https://github.com/adrighem/Conduit/commit/fcf641ce05b9f046a288313d0e68069e6c9024ea)), closes [#12](https://github.com/adrighem/Conduit/issues/12)
* **sidebar:** borrow conversation catalog reads ([130266c](https://github.com/adrighem/Conduit/commit/130266c5681003ed4df353e8de8d610defa82cd9))
* **sidebar:** update changed rows incrementally ([80e672f](https://github.com/adrighem/Conduit/commit/80e672f5840824d29fbb03be909d9ecef7db8107)), closes [#12](https://github.com/adrighem/Conduit/issues/12)

## [0.3.0](https://github.com/adrighem/Conduit/compare/v0.2.0...v0.3.0) (2026-07-29)


### Features

* **composer:** Complete person mentions ([7c88040](https://github.com/adrighem/Conduit/commit/7c88040fe45e5674c48376c951a147b4e3685e96))
* improve unread navigation and sidebar sections ([acca7a0](https://github.com/adrighem/Conduit/commit/acca7a01db6033bbb36a452d601188d69c21c833))
* **messages:** model conversation open sessions ([a9d9af3](https://github.com/adrighem/Conduit/commit/a9d9af3ddf9ff0b25fade46eb28357cfefcaf7a3))
* **messages:** reconcile conversation snapshots ([3e8c95f](https://github.com/adrighem/Conduit/commit/3e8c95f87a0c21d4959eace6762a2a9c55d40e3d))
* **messages:** route opens through immutable intent ([70eb68e](https://github.com/adrighem/Conduit/commit/70eb68e9525bf1357e7e4e1cea8629135a9034d2))
* **messages:** support rich bot messages ([15509ae](https://github.com/adrighem/Conduit/commit/15509aeaedd52bce6450d661bbfb43a9491a2269))
* **messages:** unify initial viewport control ([d96cb25](https://github.com/adrighem/Conduit/commit/d96cb253c5ba37625c307caa9017a6819b14fce1))
* **sidebar:** Add priority conversations section ([161cecd](https://github.com/adrighem/Conduit/commit/161cecdee48a2f447d0ebfda2b42f880a6ea4957))
* **sidebar:** Open DM profiles ([8947521](https://github.com/adrighem/Conduit/commit/89475215a474341e75aa042f97035024b07f9c20))
* **sidebar:** Toggle priority conversations ([8c4b604](https://github.com/adrighem/Conduit/commit/8c4b6043bb3a7afb5ff271d29925ea31532cea7f))
* **slack:** Add conversation star state ([924854d](https://github.com/adrighem/Conduit/commit/924854d356a6f3cdfcb22472d47e891ebd455a22))
* **status:** Add native status editor ([740212a](https://github.com/adrighem/Conduit/commit/740212a19b695322ea58435d29aa590587099c7a))
* **status:** Add Slack status mutation ([0b8e6d9](https://github.com/adrighem/Conduit/commit/0b8e6d949b3804a96e7373f3479cac1aae5d1c21))


### Bug Fixes

* **branding:** Remove obsolete installed icons ([f5654ef](https://github.com/adrighem/Conduit/commit/f5654ef258bc661671122e542cce6a5ea69fd141))
* **ci:** clarify collapsed section branch ([5458004](https://github.com/adrighem/Conduit/commit/54580046e3f5feacb156e955229a3286cb5f3197))
* **ci:** validate main before release automation ([abd93fc](https://github.com/adrighem/Conduit/commit/abd93fc625502de819d29895701d81ea99d0bba6))
* **deps:** remove unused HTTP/3 stack ([280bec6](https://github.com/adrighem/Conduit/commit/280bec60fcf36a40fc3b69ded60cdb1381e04994)), references [#14](https://github.com/adrighem/Conduit/issues/14)
* **messages:** satisfy strict architecture lint ([2628864](https://github.com/adrighem/Conduit/commit/26288645711383c6bb130ba56cb2a7c1f04a5095))
* **sidebar:** Keep priority stars consistent ([16a774b](https://github.com/adrighem/Conduit/commit/16a774bcf45218a630b201309a178e874f0897ee))
* **status:** Fix emoji picker search and load the full catalog ([0b074b2](https://github.com/adrighem/Conduit/commit/0b074b201f9dc9950cc98c4b2daef61828385530))
* **ui:** Fix startup interaction and animate sent messages ([982f628](https://github.com/adrighem/Conduit/commit/982f628582421ef8f87010033e84782c9f066337))
* **ui:** keep message sends responsive ([bbf9539](https://github.com/adrighem/Conduit/commit/bbf9539ca5f0b6d790adcd88d7964b19a1e44d67))

## [0.2.0](https://github.com/adrighem/Conduit/compare/v0.1.2...v0.2.0) (2026-07-23)


### Features

* **attention:** add privacy-safe runtime metrics ([4d6ed69](https://github.com/adrighem/Conduit/commit/4d6ed69d2ba798b8fbfa5263687f38aa2bbe58c9))
* **attention:** define relevance policy ([b3d14b1](https://github.com/adrighem/Conduit/commit/b3d14b1fbbd9e96be8aa1a25f99fe02e801de89e))
* **attention:** integrate canonical notification pipeline ([760fe7c](https://github.com/adrighem/Conduit/commit/760fe7c813e4b6b00c96e82104c0b9c377f29552))
* **preferences:** add live notification filters ([276dbe7](https://github.com/adrighem/Conduit/commit/276dbe73c708824c11dce7c8efdfde3a4b9121d4))


### Bug Fixes

* **runtime:** satisfy strict conversation loader lint ([e88d2f4](https://github.com/adrighem/Conduit/commit/e88d2f4310544a073e65d31c43e8011aaeab79dd))
* **sidebar:** restore unread and recent direct messages ([1da79bd](https://github.com/adrighem/Conduit/commit/1da79bddaad952576c74dafdc3d1e7221aa0c59e))

## [0.1.2](https://github.com/adrighem/Conduit/compare/v0.1.1...v0.1.2) (2026-07-22)


### Bug Fixes

* **ci:** harden release pull request checks ([c590cdb](https://github.com/adrighem/Conduit/commit/c590cdb7c84c3dd277acdda3348a00021b909a6d))
* **ci:** upgrade actions to Node 24 ([02ed3b3](https://github.com/adrighem/Conduit/commit/02ed3b3e305b2728b9fccb042d75a0984e142cbb))

## [0.1.1](https://github.com/adrighem/Conduit/compare/v0.1.0...v0.1.1) (2026-07-21)


### Bug Fixes

* stabilize the first release ([87a537c](https://github.com/adrighem/Conduit/commit/87a537c40cabbabc0ebfcc91d691c6b0325f38de))

## 0.1.0 (2026-07-21)


### Features

* add cached message avatars ([f1639cc](https://github.com/adrighem/Conduit/commit/f1639cc0b46324fc96f557bee527decd70538678))
* add conversation creation and navigation ([2a37d56](https://github.com/adrighem/Conduit/commit/2a37d5695db5dd8f1113acb887d188de46c8232e))
* **auth:** Add XOXC/XOXD login option ([ca8c1f1](https://github.com/adrighem/Conduit/commit/ca8c1f1ec1b8ca3f52cdd98edde8483e72010ac4))
* **auth:** Support XOXC/XOXD token import ([3ff62dc](https://github.com/adrighem/Conduit/commit/3ff62dc9375d56c0ba41ac63bbe03160fa8bd073))
* **history:** Prefetch recent channel history ([6f3750f](https://github.com/adrighem/Conduit/commit/6f3750f239b23657fd1e271b459efa04804a9c32))
* **history:** Reuse cached conversation history ([1c42058](https://github.com/adrighem/Conduit/commit/1c4205878e167d838d1d1c869500eb5360ec77f5))
* **huddles:** add discovery UI and Slack fallback ([9c36309](https://github.com/adrighem/Conduit/commit/9c3630953db2aa83ff381f2ce93f0fbd43bf6cdf))
* **huddles:** add GStreamer media engine ([0d2b199](https://github.com/adrighem/Conduit/commit/0d2b199130ee15bb5b6ca277cec39ce7dace9c19))
* **huddles:** add portal screen sharing ([44e131e](https://github.com/adrighem/Conduit/commit/44e131e019836f4d25dbc8bc51cdfdf54f1212c3))
* **huddles:** add session coordinator ([22707a4](https://github.com/adrighem/Conduit/commit/22707a41922efaa89987627eb76f344e518c02c7))
* **huddles:** complete packaging and validation ([39273c6](https://github.com/adrighem/Conduit/commit/39273c61bc246967f2171faaed920ae684165d16))
* **huddles:** isolate Slack Chime boundary ([0e97e1e](https://github.com/adrighem/Conduit/commit/0e97e1ef4d183af59d7e9fbc8eff73d5a9c9f56b))
* **huddles:** model Slack discovery events ([0df9f74](https://github.com/adrighem/Conduit/commit/0df9f74ce70b039936ee489c9c2f5d43f304e294))
* implement browser-session socket mode and live connection status ([e4b7e62](https://github.com/adrighem/Conduit/commit/e4b7e6270159f737036336e7092daad1bffc337f))
* **lifecycle:** define workspace transitions ([c32f890](https://github.com/adrighem/Conduit/commit/c32f890e24b8d23a92e6ace5c86e51c2385e4598))
* **lifecycle:** emit runtime transitions ([74c1daa](https://github.com/adrighem/Conduit/commit/74c1daa5304aa71840d8fe304d9ee2582664b46f))
* **lifecycle:** render workspace state ([c3703c6](https://github.com/adrighem/Conduit/commit/c3703c6f950af14a62ffced6f32a51668b6f8f2c))
* **links:** accept Slack URIs from the desktop ([6283882](https://github.com/adrighem/Conduit/commit/62838828fd77a25350a1f789f932ce0d9e8e6e7a))
* **links:** parse Slack custom URIs ([0373048](https://github.com/adrighem/Conduit/commit/0373048c8d963b86a421270a99bbb141cdac8641))
* **links:** route Slack URI targets ([3ab71a6](https://github.com/adrighem/Conduit/commit/3ab71a6e5d92a842bddbeeeadd3b8f42f3fa44a1))
* **nav:** Add Ctrl-K conversation switcher ([9588d32](https://github.com/adrighem/Conduit/commit/9588d32d07ce1d2bf35da31bab65b9f0f72254f4))
* **observability:** initialize tracing ([ca201ae](https://github.com/adrighem/Conduit/commit/ca201ae59497fb7bac327f6b4b610417eeaeda3f))
* **observability:** trace runtime work ([9d0f4c5](https://github.com/adrighem/Conduit/commit/9d0f4c5765073c9000bdcaf5daed6978cf8f2969))
* refine conversation navigation and read state ([1346797](https://github.com/adrighem/Conduit/commit/1346797d5e3d85abfc0e909657e4de8ff9bb6277))
* **release:** automate Linux package releases ([d305a9a](https://github.com/adrighem/Conduit/commit/d305a9af635aa9a2923dccda6e25fb56a9d9fdd9))
* **renderer:** Improve semantics and localization ([2794a90](https://github.com/adrighem/Conduit/commit/2794a903dd3c78d603b9fab52c2573704c012175))
* search prospective DMs from GNOME ([2602481](https://github.com/adrighem/Conduit/commit/2602481abd21eb4c52734fdbd70270c3c4ff86b1))
* **services:** add conversation history use case ([5715ae5](https://github.com/adrighem/Conduit/commit/5715ae5d2b8f1ee2516404f419d15f237e3588b6))
* **settings:** persist window and show realtime status ([18aaadb](https://github.com/adrighem/Conduit/commit/18aaadb72e48d06212c9ec721a55f208e1165730))
* show full names in DM navigation ([e406e66](https://github.com/adrighem/Conduit/commit/e406e6649a63c449656496b4a58f66f6be28f9a3))
* **sidebar:** Hide inactive conversations by default ([87b007c](https://github.com/adrighem/Conduit/commit/87b007c905f1bdb832cc1fb1a361868f819d44e0))
* **sidebar:** Load member conversations resiliently ([0daa8e9](https://github.com/adrighem/Conduit/commit/0daa8e952fd6214279a44b0984c484f891bc58a8))
* **store:** add persistent connection hub ([9e4b31a](https://github.com/adrighem/Conduit/commit/9e4b31a096d6118374e32e5a440348308e460be9))
* **store:** add schema v2 cache recovery ([4e306e2](https://github.com/adrighem/Conduit/commit/4e306e23e753edfd75fb5f98aebcb2f725c5471b))
* **store:** batch maintenance transactions ([2b255e6](https://github.com/adrighem/Conduit/commit/2b255e6d82db4a6dc16acbbabb95c8627757af44))
* **sync:** Add optional Socket Mode updates ([95fd6c5](https://github.com/adrighem/Conduit/commit/95fd6c5b8ff2802ffa5e0e88f03096ccd73627ad))
* **ui:** Add adaptive accessible workspace shell ([3076778](https://github.com/adrighem/Conduit/commit/30767782532c8c6f143b42500dbe443330933ec6))
* **ui:** discover and open conversations ([26c919f](https://github.com/adrighem/Conduit/commit/26c919f0907b86e44d8df8696a4e3e6c62f99b30))
* **ui:** localize message timestamps with Intl ([125c847](https://github.com/adrighem/Conduit/commit/125c8477a168831487295660660298f50dccf0f7))
* **ui:** rebuild message quick actions ([25a8a13](https://github.com/adrighem/Conduit/commit/25a8a138f889c35eff50c40ff7e2b4f84adeb660))
* **ux:** Complete conversation workflows ([f02d63b](https://github.com/adrighem/Conduit/commit/f02d63babc5a676dd177b94bca963463cb46a745))
* **workspace:** add canonical coordinator reducer ([5ab1767](https://github.com/adrighem/Conduit/commit/5ab1767f395553f8b4b93621a835c5f7af848701))
* **workspace:** define revisioned pipeline contracts ([918ae95](https://github.com/adrighem/Conduit/commit/918ae95e8cf2f6cd7ae0980a153fdcd0e3333776))
* **workspace:** enforce message merge invariants ([a7e44a2](https://github.com/adrighem/Conduit/commit/a7e44a22b9c531fc92d6b3ad098f176c6a57e00d))


### Bug Fixes

* **auth:** harden browser-session import ([98d41a8](https://github.com/adrighem/Conduit/commit/98d41a879b3f8bb410fca285fd788751c9653686))
* **build:** propagate Meson pkg-config paths ([3d1d196](https://github.com/adrighem/Conduit/commit/3d1d196a6c798c0eaecdc7f0e10a382994a369ee))
* **cache:** Store bounded merged channel history ([c60ae35](https://github.com/adrighem/Conduit/commit/c60ae35a3f258f49bb98e5fbd959cdd4cd57cc64))
* **ci:** Resolve stable Clippy lints ([227b099](https://github.com/adrighem/Conduit/commit/227b0993e8e618e82e72ae8fdf5319f2c0d7fe32))
* **ci:** resolve strict clippy warnings ([9c82a64](https://github.com/adrighem/Conduit/commit/9c82a64a62f15f71d1a10c2437cfd322af9048a2))
* **ci:** stabilize headless keyboard test ([229bd51](https://github.com/adrighem/Conduit/commit/229bd513a630c67c926d3258e1bdb7f2d12779e5))
* clarify initial workspace synchronization ([e3f1d08](https://github.com/adrighem/Conduit/commit/e3f1d082bb9b4a889857586b46b7dcb1e2a264ef))
* **compose:** Send messages on Enter ([c1d130a](https://github.com/adrighem/Conduit/commit/c1d130aaa1aef0f4eef7ecdb21e0f1291a6405eb))
* harden realtime session validation ([890b73a](https://github.com/adrighem/Conduit/commit/890b73a648673b781cd3e5c0c03ec0df6117c236))
* **history:** Refresh latest page after cached render ([033b3f6](https://github.com/adrighem/Conduit/commit/033b3f6a0d8a31c9cdb5aacfb9f53f5799ec8e96))
* **huddles:** satisfy native media lint ([2e3af47](https://github.com/adrighem/Conduit/commit/2e3af47acf8f7d293b1dacffde34492add04bb96))
* keep profile metadata literal ([1e6828c](https://github.com/adrighem/Conduit/commit/1e6828cb6104430673216d25ad2b928961840b31))
* make GNOME search results reliable ([0b83e49](https://github.com/adrighem/Conduit/commit/0b83e491f4b303756c6cd6b1e63bf894ac7fc368))
* **messages:** avoid no-op scroll restoration ([eac85c0](https://github.com/adrighem/Conduit/commit/eac85c0f89424e01fc6794aa1fc844b2ee2abb4e))
* **messages:** Keep timelines anchored at bottom ([cb80c3c](https://github.com/adrighem/Conduit/commit/cb80c3c66ad1c93db07570cadc428a8b3ed10504))
* **messages:** Preserve viewport when loading older history ([66910c5](https://github.com/adrighem/Conduit/commit/66910c5aa06f615e3c3cd0f2f708e841c5285ca3))
* **messages:** Run timeline scroll scripts ([f33252d](https://github.com/adrighem/Conduit/commit/f33252df9e6a9274d1dc39ce6c807712550d9db3))
* **messages:** show full names in author tooltips ([b6ff4e6](https://github.com/adrighem/Conduit/commit/b6ff4e6b2a696f805e33ca2626019160eece335b))
* **messages:** stabilize sent timeline updates ([cdb9d95](https://github.com/adrighem/Conduit/commit/cdb9d95f5986ac77715a4d51a08c2eda15013d33))
* **notifications:** include senders and open threads ([6ec75d8](https://github.com/adrighem/Conduit/commit/6ec75d866e071478fa2ea0ef316485d13487e936))
* **observability:** retain debug activation state ([f1494e5](https://github.com/adrighem/Conduit/commit/f1494e51155704bef1fbb7e9dc2475dcbf594acd))
* **observability:** write traces to stderr ([2e1f281](https://github.com/adrighem/Conduit/commit/2e1f28107a46682c9ca0ce4f0a24ee557282e061))
* **release:** install native media test plugins ([3100301](https://github.com/adrighem/Conduit/commit/310030180b16c18b414e5aa9cc1cf0ac09fde3ef))
* **release:** stabilize native package checks ([d95daa5](https://github.com/adrighem/Conduit/commit/d95daa58b7cdaded676de79f0cdace75dff84af9))
* **release:** use supported Flatpak action inputs ([651ef16](https://github.com/adrighem/Conduit/commit/651ef16269cdb1772a77b0eea0f6a5f8bde46c8c))
* render Slack HTML entities once ([65e4912](https://github.com/adrighem/Conduit/commit/65e491299c088ce77fad6be441eb6b0b2d581696))
* **runtime:** bound startup conversation enrichment ([bf14d09](https://github.com/adrighem/Conduit/commit/bf14d09f0ac1a5622240cde2710e7657b37186db))
* **runtime:** Ignore stale workspace responses ([313b05f](https://github.com/adrighem/Conduit/commit/313b05f6d8a9587c20d9298bebe5b4ca8fa194c3))
* **runtime:** Refresh conversations in background ([7d67cfc](https://github.com/adrighem/Conduit/commit/7d67cfc7f92580cc121a653d5f0b9da0f80e5834))
* **search:** enable D-Bus desktop activation ([82850ea](https://github.com/adrighem/Conduit/commit/82850ea0a4b79bf2dd1d56c6421ae08475a0a245))
* **sidebar:** Avoid user-name refresh churn ([db93e6b](https://github.com/adrighem/Conduit/commit/db93e6b351eeaf5da3fba27566826dbf945ad999))
* **sidebar:** Bold unread conversation titles ([d3f3b99](https://github.com/adrighem/Conduit/commit/d3f3b997eeb8a863a8145f1888ce92a5025d9a84))
* **sidebar:** Cache DM display names ([3570402](https://github.com/adrighem/Conduit/commit/3570402cff090a4b55ff1b4f0197f151fd0eb34c))
* **sidebar:** Keep refresh visually backgrounded ([93eec70](https://github.com/adrighem/Conduit/commit/93eec701015e5a9b153b5344210bc9d3f4f9626b))
* **sidebar:** Preserve populated list during refresh ([6eac9a8](https://github.com/adrighem/Conduit/commit/6eac9a8c62e6854b787952a29c25ff5e2c579a8f))
* stabilize first release packages ([af7ed57](https://github.com/adrighem/Conduit/commit/af7ed578302f37f013c441de5f49ef90adef69d2)), references [#14](https://github.com/adrighem/Conduit/issues/14)
* **store:** avoid whole-cache rewrites for unread updates ([d11f589](https://github.com/adrighem/Conduit/commit/d11f58911663d8447f837a02ec50de61d09f47b6))
* **store:** isolate malformed conversation rows ([2e39530](https://github.com/adrighem/Conduit/commit/2e395306d50a1cb2248022697c905198e864df4e))
* **store:** Serialize cache updates ([5cfb369](https://github.com/adrighem/Conduit/commit/5cfb369d9246721c80f46a3e5f0b7d167106abcd))
* **test:** bound headless window manager teardown ([b8dd38b](https://github.com/adrighem/Conduit/commit/b8dd38b52aae719b253db51b33350bfdc662e99d))
* **timeline:** stabilize resize anchoring ([b48bbde](https://github.com/adrighem/Conduit/commit/b48bbde563ebdba63486d416aa29f75d05c4e13a))
* **ui:** close conversation switcher with Escape ([7b8f1f5](https://github.com/adrighem/Conduit/commit/7b8f1f54ff67ef168e0cc6309ee4ea576ec964a2))
* **ui:** close message overflow after actions ([138e462](https://github.com/adrighem/Conduit/commit/138e462140970b8187200a91c481cb0e0e6c9316))
* **ui:** Recover runtime failures locally ([5c053d0](https://github.com/adrighem/Conduit/commit/5c053d0c65a6dc3bbb49a7071d768714f8503f70))
* **ui:** refine message views and notifications ([b93e743](https://github.com/adrighem/Conduit/commit/b93e7431c698437381361518dabd5267a2d9e2b2))
* **workspace:** box message patch payloads ([3d90764](https://github.com/adrighem/Conduit/commit/3d907643a921d662d1861721b0896fcf03de7639))


### Performance Improvements

* speed up GNOME search provider ([72e567b](https://github.com/adrighem/Conduit/commit/72e567b2ccfd7f179ae2f57e58ae13bd0aa2dfe1))
* **switcher:** open conversation picker immediately ([8367e55](https://github.com/adrighem/Conduit/commit/8367e55545a892a82f81ed64c050699fe669539d))
