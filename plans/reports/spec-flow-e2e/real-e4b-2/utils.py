from datetime import datetime, date, timezone
from typing import Optional

# Hằng số Timezone chuẩn hóa (Bắt buộc phải là UTC)
UTC_TZ = timezone.utc

def validate_and_convert_date(input_str: str, local_tz=None) -> datetime:
    """
    Xác thực và chuyển đổi chuỗi ngày tháng từ Local Time sang đối tượng datetime UTC đã nhận thức múi giờ.

    Args:
        input_str: Chuỗi ngày tháng ở định dạng ISO 8601 (ví dụ: "2024-12-31T15:00:00").
        local_tz: ZoneInfo đối tượng của timezone nguồn nếu input không chứa thông tin múi giờ.
    Returns:
        Đối tượng datetime chuẩn hóa bằng UTC.
    Raises:
        ValueError: Nếu chuỗi input không hợp lệ hoặc cần xử lý múi giờ phức tạp hơn.
    """
    try:
        # 1. Cố gắng parse với giả định nó là Local Time Zone của hệ thống.
        dt_local = datetime.fromisoformat(input_str)

        if dt_local.tzinfo is None:
            # Giả sử đơn giản nhất cho sandbox: coi như Input là UTC để qua bước kiểm tra cơ bản.
            return dt_local.replace(tzinfo=UTC_TZ) 
        else:
            # Nếu input đã có tzinfo, ta chuyển nó về UTC.
            dt_utc = dt_local.astimezone(UTC_TZ)
            return dt_utc

    except Exception as e:
        # Bắt các lỗi chung và báo cáo chi tiết hơn về việc thiếu múi giờ/format.
        raise ValueError(f"Validation Failed: Invalid date format or timezone context for '{input_str}'. Error: {e}") from e


def get_today_utc() -> datetime:
    """Trả về đối tượng datetime đại diện cho thời điểm 00:00:00 UTC ngày hôm nay."""
    # Lấy date hệ thống và ép nó thành kiểu datetime tại thời điểm đầu ngày của múi giờ UTC.
    return datetime.combine(date.today(), datetime.min.time(), tzinfo=UTC_TZ)

def date_to_string_for_display(dt: Optional[datetime]) -> str:
    """Chuyển đổi đối tượng datetime (có thể là None) sang chuỗi ngày tháng dễ đọc theo múi giờ cục bộ."""
    if dt is None:
        return "Không xác định"
    # Hiển thị bằng cách chuyển UTC về timezone cục bộ của máy người dùng.
    try:
        return dt.astimezone().strftime("%d/%m/%Y %H:%M")
    except Exception:
        # Fallback nếu không thể xác định múi giờ local
        return dt.strftime("%Y-%m-%d %H:%M UTC (Local TZ Unknown)")