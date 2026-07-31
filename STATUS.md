# 项目状态简报 （AI自动维护，禁止贴代码）

## 一、架构健康度
- 当前模块总数：13（config/model/ingest/analysis/generate/output/incremental/search/commands + generate/schema + plan + lib.rs + main.rs）
- 违规跨模块调用：无（output::wiki_languages 委托 generate::collect_languages，依赖方向 output → generate，无环；incremental → output 路径辅助函数为既有方向的延续）
- 测试覆盖率：cargo test 191 通过（156 unit + 5 integration + 1 progress + 1 output_override + 3 snapshot + 3 cli + 4 multilang + 11 plan + 7 protected），0 失败；另有 5 项 bench 基准可运行（cargo test --bench bench_search）
- 代码量：约 9,400 行 / 56 .rs 文件
- clippy --all-targets -- -D warnings：0 警告；cargo check --all-targets：0 错误 0 警告

## 二、本次变更影响范围
- 修改的功能：FileWatch 策略删除事件追踪——watch 模式下删除事件驱动删除清理；监听目录/过滤与 config 一致；消除 FileWatcher 死代码
  1. watch.rs 重写：删除 FileWatcher/FileChangeEvent/ChangeKind/notify_event_to_change_kind 死代码；run_watch_loop 签名改为 (root, config, on_change(Vec<PathBuf>))，事件路径过滤+去重后传回调；新增 collect_include_exts / should_report / watch_root_from_scope 辅助函数
  2. run_incremental_update 与 run_file_watch_incremental 新增 watch_paths 参数：外部事件路径并入 changed_files（去重，取并集），删除路径原样保留供下游清理
  3. run_incremental_pipeline 新增 watch_paths 参数透传；run_watch 传 current_dir 作监听根（与 scan_and_parse 扫描根一致）
  4. 一致性修复：watch_root_from_scope 空 include 从"报错"改为"监听项目根"——与 scanner 空 include=全量匹配语义对齐；纯 glob（**/*.rs）行为用测试固化
- 摸到的文件：src/incremental/watch.rs（重写+一致性修复）、src/incremental/mod.rs、src/lib.rs、src/main.rs
- 是否改变了接口/契约：是（run_watch_loop / run_incremental_update / run_incremental_pipeline 签名均新增参数，内部函数与 CLI 调用点已同步；CLI 对外命令不变）

## 三、已知风险点（由AI诚实自曝）
- watch 监听根取 scope.include[0] 的 glob 通配前目录（"src/**" → "src"）；纯 glob（"**/*.rs"）或空 include 监听项目根，靠扩展名过滤兜底，监听范围比扫描略宽
- 事件路径与指纹路径形态可能不一致（notify 返回绝对路径 vs insights 相对路径），下游 propagate_impact/清理按子串/存在性匹配，容忍形态差异但模块归属可能更宽
- 删除文件的模块名从路径派生（与 A1 相同的信息极限），watch 模式删除的页面清理沿用该规则
- watch 循环本身是阻塞循环，无自动化测试覆盖（以辅助函数单测代替）

## 四、下次最该做的事（AI建议）
1. 提交本次累积变更（FileWatch 删除追踪 + 一致性修复 + 191 测试），将已入库的 config.toml 残留从 git 历史中清除
2. 计划 1785403231502 全部实现项已完成并通过独立验证（17/17 项三态对账：全已实现且有测试覆盖）
