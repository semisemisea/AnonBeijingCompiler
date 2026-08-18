# ARM Bench

服务端在本机使用 Soyo 或 Clang 生成 AArch64 静态 ELF，再通过 HTTP 上传至评测板。
任务和用例由统一 FIFO 队列串行执行；源码、输入、预期输出和结果按 SHA-256 内容寻址
存储。板端 `soyo-benchd` 维护运行队列，使用 `CLOCK_MONOTONIC_RAW` 计时，网络耗时
不计入样本。

测试用例直接读取仓库的 `tests/{functional,h_functional,perf}` 目录。网页右上角的编辑入口
可新建、修改、移动或删除 `.sy` 及对应的可选 `.in`、`.out` 文件。
重试会在原任务中覆盖结果；任务级重试只重新执行未通过的用例。

用例文件按字节快照（与 Docker harness 的 `read_bytes()` 一致），不会做换行符转换；
`Path.read_text()` 会把 CRLF 转成 LF，曾导致 `functional/68_brainfk`（`.out` 含 CRLF）
被误判 WA。历史任务若仍复用损坏的快照，可一键刷新：

```bash
uv run python -m arm_bench.resync_cases --dry-run   # 预览
uv run python -m arm_bench.resync_cases             # 应用（幂等）
```

```bash
sudo install -m 0755 board/soyo-benchd /usr/local/bin/soyo-benchd
sudo install -m 0644 board/soyo-benchd.service /etc/systemd/system/soyo-benchd.service
sudo systemctl daemon-reload
sudo systemctl enable --now soyo-benchd
```

首次连接真板前，从现有测试镜像集中提取一份共享交叉工具链（约 59 MB，`data/` 不纳入
版本控制）：

```bash
mkdir -p data/toolchain
docker run --rm --entrypoint sh -v "$PWD/data/toolchain:/out" soyo-test-tools \
  -lc 'cp -a /usr/aarch64-linux-gnu /out/; mkdir -p /out/usr/lib/gcc-cross; cp -a /usr/lib/gcc-cross/aarch64-linux-gnu /out/usr/lib/gcc-cross/'
```

```bash
cd tools/arm-bench-web
uv sync
uv run python -m arm_bench.server
```

另开一个终端：

```bash
cd tools/arm-bench-web
pnpm install
pnpm dev
```

打开 `http://localhost:4173`。接口文档位于 `http://localhost:8765/docs`。
