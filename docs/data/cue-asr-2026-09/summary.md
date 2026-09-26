| 语种 | 变体 | 3 次结果一致 | 判定 | 转写 |
|---|---|---|---|---|
| zh | baseline | 是 | ✅ 与基线逐字相同 | '今天下午3点开会，记得带上电脑和充电器。' |
| zh | start@0 | 是 | ✅ 与基线逐字相同 | '今天下午3点开会，记得带上电脑和充电器。' |
| zh | double(start@0+upgrade@350) | 是 | ✅ 与基线逐字相同 | '今天下午3点开会，记得带上电脑和充电器。' |
| zh | start-conversation@0 | 是 | ✅ 与基线逐字相同 | '今天下午3点开会，记得带上电脑和充电器。' |
| zh | end@tail | 是 | ✅ 与基线逐字相同 | '今天下午3点开会，记得带上电脑和充电器。' |
| zh | silence-only(no cue, control) | 是 | 对照（为空） | '' |
| zh | start-only(no speech) | 是 | ✅ 为空 | '' |
| zh | double-only(no speech) | 是 | ✅ 为空 | '' |
| en | baseline | 是 | ✅ 与基线逐字相同 | 'Please remind me to call Alice tomorrow morning.' |
| en | start@0 | 是 | ✅ 与基线逐字相同 | 'Please remind me to call Alice tomorrow morning.' |
| en | double(start@0+upgrade@350) | 是 | ✅ 与基线逐字相同 | 'Please remind me to call Alice tomorrow morning.' |
| en | start-conversation@0 | 是 | ✅ 与基线逐字相同 | 'Please remind me to call Alice tomorrow morning.' |
| en | end@tail | 是 | ✅ 与基线逐字相同 | 'Please remind me to call Alice tomorrow morning.' |
| en | silence-only(no cue, control) | 是 | 对照（为空） | '' |
| en | start-only(no speech) | 是 | ✅ 为空 | '' |
| en | double-only(no speech) | 是 | ✅ 为空 | '' |
| th | baseline | 是 | ✅ 与基线逐字相同 | 'พรุ่งนี้เช้าช่วยเตือนให้โทรหาแม่ด้วย' |
| th | start@0 | 是 | ✅ 与基线逐字相同 | 'พรุ่งนี้เช้าช่วยเตือนให้โทรหาแม่ด้วย' |
| th | double(start@0+upgrade@350) | 是 | ✅ 与基线逐字相同 | 'พรุ่งนี้เช้าช่วยเตือนให้โทรหาแม่ด้วย' |
| th | start-conversation@0 | 是 | ✅ 与基线逐字相同 | 'พรุ่งนี้เช้าช่วยเตือนให้โทรหาแม่ด้วย' |
| th | end@tail | 是 | ✅ 与基线逐字相同 | 'พรุ่งนี้เช้าช่วยเตือนให้โทรหาแม่ด้วย' |
| th | silence-only(no cue, control) | 是 | 对照（纯静音本身就出字） | 'The' |
| th | start-only(no speech) | 是 | ⚠️ 出字，但对照组纯静音同样出字（既有问题） | 'ค่ะ' |
| th | double-only(no speech) | 是 | ⚠️ 出字，但对照组纯静音同样出字（既有问题） | 'and the' |

基线转写：zh=['今天下午3点开会，记得带上电脑和充电器。']；en=['Please remind me to call Alice tomorrow morning.']；th=['พรุ่งนี้เช้าช่วยเตือนให้โทรหาแม่ด้วย']
