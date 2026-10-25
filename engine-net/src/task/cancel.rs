//! 取消语义：generation 机制（旧 generation 的回调一律丢弃）。
//!
//! 规则（旧版编码规范 #3）：任务每次「启动 / 恢复 / 暂停 / 删除」都推进
//! generation；下载协程持有启动时的 generation 快照，任何进度回调、状态
//! 落库前都要先比对，不匹配即视为迟到数据，直接丢弃。

/// 推进 generation（回绕不影响判定：只做相等比较）。
pub fn bump(current: u64) -> u64 {
    current.wrapping_add(1)
}

/// 快照是否已过期。
pub fn is_stale(snapshot: u64, current: u64) -> bool {
    snapshot != current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_detection_is_equality_based() {
        assert!(!is_stale(7, 7));
        assert!(is_stale(7, 8));
        assert_eq!(bump(u64::MAX), 0);
    }
}
