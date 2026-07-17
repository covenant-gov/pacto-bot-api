from PIL import Image, ImageDraw, ImageFont

# Canvas
W, H = 960, 480
img = Image.new("RGB", (W, H), "white")
d = ImageDraw.Draw(img)

# Font fallback chain
font_paths = [
    "/System/Library/Fonts/SFNS.ttf",
    "/System/Library/Fonts/Supplemental/Arial.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
]

def load_font(size):
    for p in font_paths:
        try:
            return ImageFont.truetype(p, size)
        except (OSError, IOError):
            continue
    return ImageFont.load_default()

font16 = load_font(16)
font14 = load_font(14)

# Box positions
you_box = (80, 80, 280, 180)
squad_box = (380, 80, 580, 180)
bot_box = (680, 80, 880, 180)

def draw_box(box, fill, outline="#1e1e1e", width=2, radius=8):
    d.rounded_rectangle(box, radius=radius, fill=fill, outline=outline, width=width)

def center_text(x, y, text, font, fill="#1e1e1e"):
    bbox = d.textbbox((0, 0), text, font=font)
    w = bbox[2] - bbox[0]
    d.text((x - w // 2, y), text, font=font, fill=fill)

def arrow(x1, y1, x2, y2, color, width=2, head_size=10):
    d.line((x1, y1, x2, y2), fill=color, width=width)
    # Simple arrowhead: two lines from tip
    import math
    angle = math.atan2(y2 - y1, x2 - x1)
    a1 = angle + math.radians(150)
    a2 = angle - math.radians(150)
    p1 = (x2 + head_size * math.cos(a1), y2 + head_size * math.sin(a1))
    p2 = (x2 + head_size * math.cos(a2), y2 + head_size * math.sin(a2))
    d.line((x2, y2, p1[0], p1[1]), fill=color, width=width)
    d.line((x2, y2, p2[0], p2[1]), fill=color, width=width)

# Draw boxes
draw_box(you_box, "#ffffff")
draw_box(squad_box, "#e7f5ff")
draw_box(bot_box, "#d0fff4")

# Box text
center_text(180, 115, "You", font16)
center_text(180, 140, "(Squad admin)", font16)
center_text(480, 115, "Pacto Squad", font16)
center_text(480, 140, "(MLS group)", font16)
center_text(780, 115, "Bot handler", font16)
center_text(780, 140, "(Python SDK)", font16)

# Arrow 1: You -> Squad
arrow(280, 130, 380, 130, "#1971c2")
center_text(330, 95, "1. Invite bot", font14, "#1971c2")

# Arrow 2: Squad -> Bot
arrow(580, 130, 680, 130, "#087f5b")
center_text(630, 95, "2. Bot joins", font14, "#087f5b")

# Arrow 3: Bot -> Squad
arrow(780, 180, 580, 260, "#e8590c")
center_text(680, 240, "3. Bot says hello", font14, "#e8590c")
center_text(680, 260, '"I\'m {bot_id}!"', font14, "#e8590c")

# Note
d.text((120, 340), "You invite the bot → it joins the Squad → it automatically announces itself.", font=font16, fill="#495057")
d.text((120, 370), "No manual hello needed. The message is customizable in the bot's code.", font=font16, fill="#495057")

img.save("bot-squad-join-flow.png")
print("saved bot-squad-join-flow.png")
