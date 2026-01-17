#!/bin/bash

# Music163bot-Rust 启动脚本

# 检查配置文件是否存在
if [ ! -f "config.ini" ]; then
    echo "配置文件 config.ini 不存在!"
    echo "请复制 config.ini.example 为 config.ini 并配置你的Bot Token"
    exit 1
fi

# 检查可执行文件是否存在
if [ ! -f "target/release/music163bot-rust" ]; then
    echo "可执行文件不存在，正在构建..."
    cargo build --release
    if [ $? -ne 0 ]; then
        echo "构建失败!"
        exit 1
    fi
fi

echo "启动 Music163bot-Rust..."
exec ./target/release/music163bot-rust "$@"
