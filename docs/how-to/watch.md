# 运维指南：watch 常驻托管

`watch` 进程自带崩溃自愈循环（v36 起：出错后 5s 起、倍增至 60s 上限退避重试；Ctrl-C 优雅退出）。进程被系统终止（关机/崩溃/被杀）时仍需要「常驻 + 自动重启」——按平台托管（watch 启动时先全量生成再监听，重启后自动收敛，不会损坏产物）：

## Linux（systemd 用户服务）

`~/.config/systemd/user/code-repo-wiki-watch.service`

```ini
[Unit]
Description=code-repo-wiki watch daemon

[Service]
ExecStart=/absolute/path/to/code-repo-wiki watch
WorkingDirectory=/absolute/path/to/project
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

启用：`systemctl --user enable --now code-repo-wiki-watch`

## macOS（launchd LaunchAgent）

`~/Library/LaunchAgents/com.code-repo-wiki.watch.plist`

```xml
<plist version="1.0"><dict>
  <key>Label</key><string>com.code-repo-wiki.watch</string>
  <key>ProgramArguments</key>
  <array><string>/absolute/path/to/code-repo-wiki</string><string>watch</string></array>
  <key>WorkingDirectory</key><string>/absolute/path/to/project</string>
  <key>KeepAlive</key><true/>
</dict></plist>
```

加载：`launchctl load ~/Library/LaunchAgents/com.code-repo-wiki.watch.plist`

## Windows（任务计划程序）

登录触发 + 失败重启（`RestartCount` 与 `RestartInterval` 需同时设置）：

```powershell
$action  = New-ScheduledTaskAction -Execute 'D:\path\to\code-repo-wiki.exe' -Argument 'watch' -WorkingDirectory 'D:\path\to\project'
$trigger = New-ScheduledTaskTrigger -AtLogOn
$settings = New-ScheduledTaskSettingsSet -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName 'code-repo-wiki-watch' -Action $action -Trigger $trigger -Settings $settings
```

## 与 git hooks 的关系

watch 与 post-commit hook 各自独立触发增量。两者并存无害（单实例运行锁保证并发不互踩，见[限制项](../reference/limitations.md)）；一般二选一即可——本地开发常用 watch，多人协作/CI 场景 hook 更稳。
