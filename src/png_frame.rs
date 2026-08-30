//! 背景图分片重组。纯逻辑,不依赖 ESP-IDF —— 经 `src/lib.rs` 暴露给宿主 target 做单测。
//!
//! BLE 的 characteristic write 只给到一串分片,本身不带「这次传输从哪开始、到哪结束」的
//! 信息。旧实现靠 `chunk.len() < 512` 判断结束,并且从不复位缓冲,导致三种失败:
//!
//! - 上传中断后重传 → 残片与完整文件被拼接,末片仍 <512 于是被判为完成,损坏数据落 NVS;
//! - 不重启连传两张图 → 同样拼接;
//! - 文件大小恰为分片大小的整数倍 → 末片不短,永远判不出结束,图片静默失效。
//!
//! 这里改用 PNG 自身的格式边界当定界符,因此**不需要改动 BLE 协议或 setup.html**:
//! 签名标记一次新传输的开始,IEND 块标记结束。对任意分片大小都成立。

// 本文件同时被编进 lib(供宿主单测)和固件 bin。bin 只用到 push/take_image,
// 其余查询方法是给测试和将来的调用方留的接口,在 bin 里必然「未使用」。
#![allow(dead_code)]

/// PNG 签名(固定 8 字节)。一个分片以它开头,即意味着新传输开始。
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// PNG 的收尾:长度为 0 的 IEND 块,其 CRC 是常量,所以整段 12 字节固定不变。
/// 以它结尾才算收全 —— 这是真实的结束标记,不是长度启发式。
const PNG_TRAILER: &[u8] = b"\x00\x00\x00\x00IEND\xaeB`\x82";

/// 背景图大小上限,与 NVS 里能存下的量级对齐。
pub const MAX_PNG_BYTES: usize = 1024 * 1024;

/// 一次 `push` 之后的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// 还在收,尚未见到结束标记。
    Accumulating,
    /// 已收全一张完整 PNG。
    Complete,
    /// 这次传输作废,缓冲已清空;附带可直接展示给用户的原因。
    Rejected(RejectReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// 分片为空。
    EmptyChunk,
    /// 累积长度超过 [`MAX_PNG_BYTES`]。
    TooLarge,
    /// 首个分片不是 PNG 签名开头 —— 要么不是 PNG,要么漏掉了传输的开头。
    NotPng,
}

impl RejectReason {
    /// 适合显示在 LCD 上的短提示。
    pub fn as_str(self) -> &'static str {
        match self {
            RejectReason::EmptyChunk => "Background: empty data",
            RejectReason::TooLarge => "Background: image too large",
            RejectReason::NotPng => "Background: not a PNG",
        }
    }
}

/// 按分片重组一张 PNG。
#[derive(Debug, Default)]
pub struct PngAccumulator {
    buf: Vec<u8>,
    complete: bool,
}

impl PngAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// 收下一个分片。
    ///
    /// 以 PNG 签名开头的分片会先清空缓冲 —— 这正是修掉「重传拼接」的地方:上一次没传完
    /// 留下的残片在这里被丢弃,而不是被续写。
    pub fn push(&mut self, chunk: &[u8]) -> PushOutcome {
        if chunk.is_empty() {
            return PushOutcome::Rejected(RejectReason::EmptyChunk);
        }

        if chunk.starts_with(PNG_SIGNATURE) {
            self.reset();
        } else if self.buf.is_empty() {
            // 没有在传输中,又不是以签名开头 —— 无从判断这是什么,不要开始累积。
            return PushOutcome::Rejected(RejectReason::NotPng);
        }

        if self.buf.len() + chunk.len() > MAX_PNG_BYTES {
            self.reset();
            return PushOutcome::Rejected(RejectReason::TooLarge);
        }

        self.buf.extend_from_slice(chunk);
        // 完整的 PNG 必以 IEND 块收尾;签名 + 收尾都齐了才算一张图。
        self.complete = self.buf.len() > PNG_SIGNATURE.len() && self.buf.ends_with(PNG_TRAILER);

        if self.complete {
            PushOutcome::Complete
        } else {
            PushOutcome::Accumulating
        }
    }

    /// 是否已收全一张完整 PNG。
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// 已收全时取出图片数据,否则 `None` —— 调用方无法误用半张图。
    pub fn image(&self) -> Option<&[u8]> {
        if self.complete {
            Some(&self.buf)
        } else {
            None
        }
    }

    /// 已收全时把图片数据**移动**出来并复位;否则 `None`。
    ///
    /// 用移动而非克隆:背景图上限 1MB,在 ESP32 上多留一份副本不值当。
    pub fn take_image(&mut self) -> Option<Vec<u8>> {
        if !self.complete {
            return None;
        }
        self.complete = false;
        Some(std::mem::take(&mut self.buf))
    }

    /// 当前累积的字节数(用于日志/进度)。
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.complete = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一张结构上完整的 PNG:签名 + 指定长度的填充 + IEND 收尾。
    /// 这里只关心分片重组,不关心像素内容,所以中段填充即可。
    fn fake_png(padding: usize) -> Vec<u8> {
        let mut v = Vec::from(PNG_SIGNATURE);
        v.extend(std::iter::repeat_n(0xAB, padding));
        v.extend_from_slice(PNG_TRAILER);
        v
    }

    /// 按 setup.html 的做法切片:固定 512 字节,只有最后一片可能短。
    fn chunks(data: &[u8]) -> Vec<&[u8]> {
        data.chunks(512).collect()
    }

    fn feed(acc: &mut PngAccumulator, data: &[u8]) -> PushOutcome {
        let mut last = PushOutcome::Accumulating;
        for c in chunks(data) {
            last = acc.push(c);
        }
        last
    }

    #[test]
    fn clean_transfer_completes_and_roundtrips() {
        let png = fake_png(3000);
        let mut acc = PngAccumulator::new();
        assert_eq!(feed(&mut acc, &png), PushOutcome::Complete);
        assert_eq!(acc.image(), Some(png.as_slice()));
    }

    /// 旧实现在这里静默失效:末片正好 512 字节,`len() < 512` 永远不成立。
    #[test]
    fn size_exactly_multiple_of_chunk_still_completes() {
        // 先凑出一个总长度为 512 整数倍的 PNG。
        let overhead = PNG_SIGNATURE.len() + PNG_TRAILER.len();
        let png = fake_png(512 * 4 - overhead);
        assert_eq!(png.len() % 512, 0, "本用例的前提:总长恰为 512 的整数倍");

        let mut acc = PngAccumulator::new();
        assert_eq!(feed(&mut acc, &png), PushOutcome::Complete);
        assert_eq!(acc.image(), Some(png.as_slice()));
    }

    /// 旧实现在这里变砖:残片 + 完整文件被拼接后仍判为完成。
    #[test]
    fn interrupted_then_retried_keeps_only_the_retry() {
        let png = fake_png(2000);
        let mut acc = PngAccumulator::new();

        // 第一次:传一半就断了。
        for c in chunks(&png).into_iter().take(2) {
            acc.push(c);
        }
        assert!(!acc.is_complete());
        assert!(!acc.is_empty(), "残片确实留在缓冲里");

        // 第二次:完整重传 —— 签名开头把残片冲掉。
        assert_eq!(feed(&mut acc, &png), PushOutcome::Complete);
        assert_eq!(acc.image(), Some(png.as_slice()), "结果只含重传的那一份");
    }

    #[test]
    fn take_image_moves_out_and_resets() {
        let png = fake_png(1000);
        let mut acc = PngAccumulator::new();
        feed(&mut acc, &png);
        assert_eq!(acc.take_image().as_deref(), Some(png.as_slice()));
        assert!(acc.is_empty(), "取走后缓冲清空");
        assert!(!acc.is_complete());
        assert_eq!(acc.take_image(), None, "不能取第二次");
    }

    #[test]
    fn two_images_back_to_back_keep_only_the_second() {
        let first = fake_png(1000);
        let second = fake_png(2500);
        let mut acc = PngAccumulator::new();

        assert_eq!(feed(&mut acc, &first), PushOutcome::Complete);
        assert_eq!(feed(&mut acc, &second), PushOutcome::Complete);
        assert_eq!(acc.image(), Some(second.as_slice()));
    }

    #[test]
    fn truncated_never_completes() {
        let png = fake_png(3000);
        let all = chunks(&png);
        let mut acc = PngAccumulator::new();
        for c in all.iter().take(all.len() - 1) {
            assert_eq!(acc.push(c), PushOutcome::Accumulating);
        }
        assert!(!acc.is_complete());
        assert_eq!(acc.image(), None, "半张图取不出来");
    }

    #[test]
    fn oversize_is_rejected_and_clears() {
        let mut acc = PngAccumulator::new();
        acc.push(PNG_SIGNATURE);
        let big = vec![0u8; MAX_PNG_BYTES];
        assert_eq!(
            acc.push(&big),
            PushOutcome::Rejected(RejectReason::TooLarge)
        );
        assert!(acc.is_empty(), "作废后缓冲要清干净,不能留给下一次拼接");
    }

    #[test]
    fn non_png_start_is_rejected() {
        let mut acc = PngAccumulator::new();
        assert_eq!(
            acc.push(b"GIF89a and then some"),
            PushOutcome::Rejected(RejectReason::NotPng)
        );
        assert!(acc.is_empty());
    }

    #[test]
    fn empty_chunk_is_rejected_without_disturbing_buffer() {
        let png = fake_png(1000);
        let mut acc = PngAccumulator::new();
        acc.push(chunks(&png)[0]);
        let before = acc.len();
        assert_eq!(
            acc.push(&[]),
            PushOutcome::Rejected(RejectReason::EmptyChunk)
        );
        assert_eq!(acc.len(), before, "空分片不该影响正在进行的传输");
    }

    /// 签名孤零零一片、后面没有内容,不能算完成。
    #[test]
    fn signature_alone_is_not_complete() {
        let mut acc = PngAccumulator::new();
        assert_eq!(acc.push(PNG_SIGNATURE), PushOutcome::Accumulating);
        assert!(!acc.is_complete());
    }
}
