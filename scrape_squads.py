"""
Scrape current rugby squads from Wikipedia for the national teams listed,
and produce squads.json for the quiz app.

Run this on a machine with internet access. Requires: requests, beautifulsoup4

    pip install requests beautifulsoup4

Usage:
    python scrape_squads.py

Output:
    squads.json --
      {
        "South Africa": {
          "players": [
            {"name": "...", "position": "...", "club": "...", "caps": "..."},
            ...
          ]
        },
        ...
      }
"""

import json
import re
import time
import requests
from bs4 import BeautifulSoup

API_URL = "https://en.wikipedia.org/w/api.php"
HEADERS = {
    "User-Agent": "RugbyQuizApp/1.0 (educational project; contact: example@example.com)"
}

TEAMS = {
    "South Africa": "South Africa national rugby union team",
    "New Zealand": "New Zealand national rugby union team",
    "Ireland": "Ireland national rugby union team",
    "France": "France national rugby union team",
    "Argentina": "Argentina national rugby union team",
    "England": "England national rugby union team",
    "Scotland": "Scotland national rugby union team",
    "Australia": "Australia national rugby union team",
    "Fiji": "Fiji national rugby union team",
    "Italy": "Italy national rugby union team",
    "Wales": "Wales national rugby union team",
    "Japan": "Japan national rugby union team",
    "Georgia": "Georgia national rugby union team",
    "Portugal": "Portugal national rugby union team",
    "Uruguay": "Uruguay national rugby union team",
    "USA": "United States men's national rugby union team",
    "Spain": "Spain national rugby union team",
    "Chile": "Chile national rugby union team",
    "Tonga": "Tonga national rugby union team",
    "Samoa": "Samoa national rugby union team",
    "Belgium": "Belgium national rugby union team",
    "Romania": "Romania national rugby union team",
    "Hong Kong China": "Hong Kong national rugby union team",
    "Zimbabwe": "Zimbabwe national rugby union team",
    "Canada": "Canada national rugby union team",
    "Namibia": "Namibia national rugby union team",
}

SQUAD_HEADING_CANDIDATES = [
    "current squad",
    "current squad and recent call-ups",
    "squad",
]

# All header keyword variants used across Wikipedia rugby squad tables
# for each column type.
NAME_KEYWORDS  = ["player", "name"]
POS_KEYWORDS   = ["position", "pos"]
CAPS_KEYWORDS  = ["cap", "apps", "appearances"]
CLUB_KEYWORDS  = [
    "club/province", "club / province", "club/prov",
    "club", "province", "region", "franchise", "team",
    "super rugby", "super14", "super 14", "super15", "super 15",
    "union", "side", "employer", "current club",
    "provincial", "state", "super", "domestic",
]


def clean_text(s):
    s = re.sub(r"\[.*?\]", "", s)   # remove footnote refs [a], [1]
    s = re.sub(r"\s+", " ", s)
    return s.strip()


def get_sections(title):
    params = {
        "action": "parse",
        "page": title,
        "prop": "sections",
        "format": "json",
    }
    r = requests.get(API_URL, params=params, headers=HEADERS, timeout=30)
    r.raise_for_status()
    data = r.json()
    if "error" in data:
        raise RuntimeError(f"{title}: API error {data['error']}")
    return data["parse"]["sections"]


def find_squad_section_index(sections):
    best = None
    players_idx = None
    for sec in sections:
        line = re.sub(r"<.*?>", "", sec["line"]).lower()
        if "players" in line:
            players_idx = sec["index"]
        for cand in SQUAD_HEADING_CANDIDATES:
            if cand in line:
                best = sec["index"]
    return best or players_idx


def get_section_html(title, index):
    params = {
        "action": "parse",
        "page": title,
        "prop": "text",
        "section": index,
        "format": "json",
    }
    r = requests.get(API_URL, params=params, headers=HEADERS, timeout=30)
    r.raise_for_status()
    data = r.json()
    if "error" in data:
        raise RuntimeError(f"{title}: API error {data['error']}")
    return data["parse"]["text"]["*"]


def find_col(headers, keywords):
    """Return the first column index whose header contains any keyword."""
    for i, h in enumerate(headers):
        for kw in keywords:
            if kw in h:
                return i
    return None


def extract_headers(table):
    """
    Return a flat list of lowercased header strings for the table,
    handling multi-row headers by merging rows until we have non-empty
    content in most cells. Wikipedia squad tables sometimes put the real
    column labels in the second header row.
    """
    rows = table.find_all("tr")
    for row in rows[:3]:   # check first 3 rows for header content
        cells = row.find_all(["th", "td"])
        if not cells:
            continue
        headers = [clean_text(c.get_text()).lower() for c in cells]
        # A useful header row has at least one of our known keywords
        all_text = " ".join(headers)
        if any(kw in all_text for kw in NAME_KEYWORDS + POS_KEYWORDS):
            return headers
    return []


def parse_squad_table(html):
    soup = BeautifulSoup(html, "html.parser")
    tables = soup.find_all("table", class_=lambda c: c and "wikitable" in c)

    players = []
    for table in tables:
        headers = extract_headers(table)
        if not headers:
            continue

        name_col = find_col(headers, NAME_KEYWORDS)
        pos_col  = find_col(headers, POS_KEYWORDS)
        caps_col = find_col(headers, CAPS_KEYWORDS)
        club_col = find_col(headers, CLUB_KEYWORDS)

        if name_col is None or pos_col is None:
            continue  # not a squad table

        # Data rows: skip rows that are all-th (sub-headers) or have
        # too few cells
        min_cols = max(c for c in [name_col, pos_col, caps_col, club_col] if c is not None) + 1
        for row in table.find_all("tr")[1:]:
            cells = row.find_all(["th", "td"])
            if len(cells) < min_cols:
                continue
            # Skip rows where all cells are <th> — position group headers
            if all(c.name == "th" for c in cells):
                continue

            # Name
            name_cell = cells[name_col]
            link = name_cell.find("a")
            name = clean_text(link.get_text()) if link else clean_text(name_cell.get_text())

            # Position
            position = clean_text(cells[pos_col].get_text())

            # Caps
            caps = ""
            if caps_col is not None and caps_col < len(cells):
                caps = clean_text(cells[caps_col].get_text())

            # Club — prefer the linked text (actual club name, not flag/country)
            club = ""
            if club_col is not None and club_col < len(cells):
                club_cell = cells[club_col]
                # Some cells have multiple links (flag + club name);
                # pick the last <a> which is almost always the club name.
                links = club_cell.find_all("a")
                if links:
                    # Skip flag/country links (their href contains "/wiki/Flag_of"
                    # or the text is very short like a country code)
                    club_links = [l for l in links if "Flag_of" not in l.get("href", "")
                                  and len(l.get_text(strip=True)) > 2]
                    if club_links:
                        club = clean_text(club_links[-1].get_text())
                    else:
                        club = clean_text(links[-1].get_text())
                else:
                    club = clean_text(club_cell.get_text())

            if not name or not position:
                continue
            if name.lower() in ("player", "name", "—", "-", ""):
                continue

            players.append({
                "name": name,
                "position": position,
                "club": club,
                "caps": caps,
            })

        if players:
            break

    return players


def scrape_team(team_name, page_title):
    sections = get_sections(page_title)
    idx = find_squad_section_index(sections)
    if idx is None:
        raise RuntimeError(f"Could not find Players/Current squad section")

    html = get_section_html(page_title, idx)
    players = parse_squad_table(html)

    if not players:
        # Try sub-sections of Players
        for sec in sections:
            line = re.sub(r"<.*?>", "", sec["line"]).lower()
            if any(c in line for c in SQUAD_HEADING_CANDIDATES):
                html2 = get_section_html(page_title, sec["index"])
                players = parse_squad_table(html2)
                if players:
                    break

    return players


def main():
    results = {}
    for team_name, page_title in TEAMS.items():
        print(f"\nScraping {team_name} ...")
        try:
            players = scrape_team(team_name, page_title)
            print(f"  -> {len(players)} players found")
            if players:
                clubs_found = sum(1 for p in players if p["club"])
                caps_found  = sum(1 for p in players if p["caps"])
                print(f"     clubs: {clubs_found}/{len(players)}  caps: {caps_found}/{len(players)}")
            results[team_name] = {"players": players}
        except Exception as e:
            print(f"  !! FAILED: {e}")
            results[team_name] = {"players": [], "error": str(e)}
        time.sleep(1)

    with open("squads.json", "w", encoding="utf-8") as f:
        json.dump(results, f, indent=2, ensure_ascii=False)

    print("\n\nDone. Wrote squads.json")
    for team, data in results.items():
        p = data["players"]
        if not p:
            print(f"  WARNING: {team} has 0 players")
        elif not any(x["club"] for x in p):
            print(f"  WARNING: {team} — no club data found")


if __name__ == "__main__":
    main()
