victory 单文件版 · Windows x64
================================

把 GTA6（Scarlett 版本体）通过 victory 跑起来。整个包只有一个 exe 需要安装：
不需要 VC++ 运行库，不需要单独的 D3D12Core.dll，程序会在第一次运行时自己
把内嵌的 D3D12 Agility SDK 620 核心写到磁盘上。


包内文件
--------
  victory.exe      主程序（16.7 MB，内嵌 D3D12 运行时，CRT 静态链接）
  Play-GTA6.cmd    启动器
  victory.env.cmd  74 个 VICTORY_* 开关（当前最优配置，不要删）
  README.txt       本文件


怎么启动（任选一种）
--------------------
  1. 把整个文件夹放进游戏目录（和 game_scarlett_master.exe 同级），双击 Play-GTA6.cmd
  2. 把游戏本体的 game_scarlett_master.exe 拖到 Play-GTA6.cmd 上
  3. Play-GTA6.cmd "D:\GTA6.Xbox\game_scarlett_master.exe"
  4. Play-GTA6.cmd "D:\GTA6.Xbox"        （给目录也行）

第一次启动会自动翻译并缓存着色器：约 26,000 个、80 秒、峰值 6 GB 内存，
完成后在目录里写一个 .shaders-warm 标记，以后不再重复。这一步失败也不影响
启动，只是开局那几分钟边跑边编译，卡顿明显。


需要什么
--------
  * Windows 10 1809（build 17763）或更新，或 Windows 11，64 位
  * 游戏本体一份（这个包不含游戏数据）
  * 显卡驱动正常（NVIDIA / AMD 近两年的驱动都行）
  * 程序所在目录可写；不可写时运行时会落到
    %LOCALAPPDATA%\victory\d3d12\620\，一样能跑


显卡与画质
----------
启动器会按显存自动设 VICTORY_TEXTURE_MIP_DROP：
  11 GB 以上 → 0（不丢 mip）
  9–11 GB   → 1（丢一级）
  9 GB 以下 → 2（丢两级，画质降、显存省）
想自己定：先 set VICTORY_KEEP_MIP_DROP=1，再设 VICTORY_TEXTURE_MIP_DROP=0/1/2。


出问题时
--------
  * 报 “D3D12 in-box runtime” —— 系统自带的 D3D12 撑不起 SM 6.6 的根签名。
    检查 victory.exe 旁边有没有 D3D12\D3D12Core.dll（第一次运行会自己生成），
    目录是否可写；也可以手动 set VICTORY_D3D12_SDK=0 强制用系统运行时，
    但那样大概率建管线失败。
  * 画面糊、贴图糊 —— MIP_DROP 太大，改回 0 或 1。
  * 显存不够、闪退 —— 把 MIP_DROP 调到 2。
  * 帧数低 —— 这版每帧要把 title 写的几十 MB 由 CPU 拷进上传内存，4 GB/s 是
    硬件墙；换成 Windows 11 只是让这条路上限高一点。
  * 日志在当前目录的 runs\ 下；`victory.exe config` 可以看当前用的是哪个
    D3D12 运行时、游戏目录、缓存目录。
