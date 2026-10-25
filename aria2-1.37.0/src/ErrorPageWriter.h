/* <!-- copyright */
/*
 * aria2 - The high speed download utility
 *
 * Copyright (C) 2026 Aria2-Harness
 *
 * This program is free software; you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation; either version 2 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program; if not, write to the Free Software
 * Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA 02110-1301 USA
 */
/* copyright --> */
#ifndef D_ERROR_PAGE_WRITER_H
#define D_ERROR_PAGE_WRITER_H

#include "common.h"

#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <ctime>
#include <string>
#include <utility>
#include <vector>

#include <dirent.h>
#include <sys/stat.h>

#include "File.h"
#include "LogFactory.h"
#include "Logger.h"
#include "Option.h"
#include "fmt.h"
#include "prefs.h"
#include "util.h"

namespace aria2 {
namespace error_page {

// Hard limits demanded by the UI contract: an error page is a diagnostic
// artifact, never a download.  256 KiB is enough for any real 4xx body and
// keeps both the RSS spike and the on-disk footprint bounded.
static const size_t MAX_BYTES = 256 * 1024;
// data/error_pages/ is a rolling window, not a log archive.
static const size_t MAX_FILES = 20;

inline std::string trimTrailingSeparators(const std::string& path)
{
  std::string result = path;
  while (result.size() > 1) {
    const char last = result[result.size() - 1];
    if (last != '/' && last != '\\') {
      break;
    }
    // Keep a drive root ("E:\" / "E:/") intact.
    if (result.size() == 3 && result[1] == ':') {
      break;
    }
    result.erase(result.size() - 1);
  }
  return result;
}

// Resolves data/error_pages/ from the --dir option.  The page directory is a
// *sibling* of the download directory (parent-of-download + data/error_pages),
// so it can never collide with user downloads even when --dir is a relative
// path.  ARIA2_HARNESS_ERROR_PAGE_DIR is an explicit test/ops override.
inline std::string getDir(const Option* option)
{
  const char* env = getenv("ARIA2_HARNESS_ERROR_PAGE_DIR");
  if (env && *env) {
    return trimTrailingSeparators(env);
  }

  std::string downloadDir = option->get(PREF_DIR);
  if (downloadDir.empty()) {
    downloadDir = ".";
  }
  File file(trimTrailingSeparators(downloadDir));
  std::string parent = file.getDirname();
  if (parent.empty()) {
    parent = ".";
  }
  return util::applyDir(parent, "data/error_pages");
}

// Keeps at most |maxFiles| *.html files in |dir|, deleting the oldest ones
// first.  Best effort by design: failing to prune must never fail a download.
inline void prune(const std::string& dir, size_t maxFiles = MAX_FILES)
{
  DIR* d = opendir(dir.c_str());
  if (!d) {
    return;
  }

  std::vector<std::pair<time_t, std::string>> files;
  while (struct dirent* ent = readdir(d)) {
    const std::string name(ent->d_name);
    if (name.size() < 6) {
      continue;
    }
    if (name.compare(name.size() - 5, 5, ".html") != 0) {
      continue;
    }
    const std::string full = util::applyDir(dir, name);
    struct stat st;
    if (stat(full.c_str(), &st) == 0 && S_ISREG(st.st_mode)) {
      files.push_back(std::make_pair(st.st_mtime, full));
    }
  }
  closedir(d);

  if (files.size() <= maxFiles) {
    return;
  }

  std::sort(files.begin(), files.end());
  const size_t removeCount = files.size() - maxFiles;
  for (size_t i = 0; i < removeCount; ++i) {
    std::remove(files[i].second.c_str());
  }
  A2_LOG_INFO(fmt("Pruned %lu old HTTP error page(s) from %s",
                  static_cast<unsigned long>(removeCount), dir.c_str()));
}

// Streams an HTTP error body to disk.  The file is created lazily on the first
// byte so a body-less 4xx does not leave an empty artifact behind, and writes
// stop dead at MAX_BYTES.
class Writer {
public:
  Writer(std::string dir, std::string gid)
      : dir_(std::move(dir)),
        gid_(std::move(gid)),
        maxBytes_(MAX_BYTES),
        written_(0),
        full_(false),
        dirReady_(false),
        fp_(nullptr)
  {
    File d(dir_);
    if (!d.isDir()) {
      d.mkdirs();
    }
    dirReady_ = d.isDir();
    if (!dirReady_) {
      A2_LOG_WARN(fmt("Could not create HTTP error page directory %s",
                      dir_.c_str()));
    }
  }

  ~Writer()
  {
    close();
    if (dirReady_) {
      prune(dir_, MAX_FILES);
    }
  }

  void write(const unsigned char* data, size_t len)
  {
    if (len == 0 || full_) {
      return;
    }
    if (!openFile()) {
      return;
    }
    const size_t remaining = maxBytes_ - written_;
    const size_t n = std::min(len, remaining);
    if (n > 0 && fwrite(data, 1, n, fp_) == n) {
      written_ += n;
    }
    else if (n > 0) {
      // Disk full / permission revoked mid-flight: stop touching the file.
      full_ = true;
      dirReady_ = false;
    }
    if (written_ >= maxBytes_) {
      full_ = true;
    }
  }

  bool full() const { return full_; }

  bool active() const { return dirReady_; }

  size_t written() const { return written_; }

  const std::string& path() const { return path_; }

private:
  bool openFile()
  {
    if (fp_) {
      return true;
    }
    if (!dirReady_) {
      return false;
    }
    path_ = util::applyDir(dir_, std::string("gid_") + gid_ + ".html");
    fp_ = fopen(path_.c_str(), "wb");
    if (!fp_) {
      A2_LOG_WARN(fmt("Could not create HTTP error page %s", path_.c_str()));
      dirReady_ = false;
      return false;
    }
    return true;
  }

  void close()
  {
    if (!fp_) {
      return;
    }
    fclose(fp_);
    fp_ = nullptr;
    A2_LOG_NOTICE(fmt("Saved HTTP error page to %s (%lu bytes)",
                      path_.c_str(),
                      static_cast<unsigned long>(written_)));
  }

  std::string dir_;
  std::string gid_;
  std::string path_;
  size_t maxBytes_;
  size_t written_;
  bool full_;
  bool dirReady_;
  FILE* fp_;

  Writer(const Writer&);
  Writer& operator=(const Writer&);
};

} // namespace error_page
} // namespace aria2

#endif // D_ERROR_PAGE_WRITER_H
