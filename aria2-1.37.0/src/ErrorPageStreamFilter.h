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
#ifndef D_ERROR_PAGE_STREAM_FILTER_H
#define D_ERROR_PAGE_STREAM_FILTER_H

#include "common.h"

#include <memory>
#include <string>

#include "DlAbortEx.h"
#include "ErrorPageWriter.h"
#include "StreamFilter.h"

namespace aria2 {

// Captures a 4xx response body on its way to the "discard" sink used by
// HttpSkipResponseCommand.
//
// It is installed as the *innermost* delegate of the transfer/content encoding
// chain, so it observes decoded entity bytes rather than raw chunked framing.
// |expectedLength| is the entity length when no encoding filter sits above
// this one, or -1 when the surrounding filter decides completion (chunked /
// gzip).  Once the 256 KiB cap is reached the filter aborts the body transfer
// immediately instead of draining a potentially unbounded error page.
class ErrorPageStreamFilter : public StreamFilter {
public:
  ErrorPageStreamFilter(std::unique_ptr<error_page::Writer> writer,
                        int64_t expectedLength = -1)
      : writer_(std::move(writer)),
        expectedLength_(expectedLength),
        bytesIn_(0),
        bytesProcessed_(0)
  {
  }

  virtual void init() CXX11_OVERRIDE
  {
    bytesIn_ = 0;
    bytesProcessed_ = 0;
  }

  virtual ssize_t transform(const std::shared_ptr<BinaryStream>& out,
                            const std::shared_ptr<Segment>& segment,
                            const unsigned char* inbuf,
                            size_t inlen) CXX11_OVERRIDE
  {
    bytesProcessed_ = inlen;
    bytesIn_ += inlen;
    if (writer_) {
      writer_->write(inbuf, inlen);
      if (writer_->full()) {
        // Caught by HttpSkipResponseCommand::executeInternal(), which then
        // runs processResponse() and raises the real status-code error.
        throw DL_ABORT_EX("HTTP error page capture limit reached");
      }
    }
    return static_cast<ssize_t>(inlen);
  }

  virtual bool finished() CXX11_OVERRIDE
  {
    if (writer_ && writer_->full()) {
      return true;
    }
    return expectedLength_ >= 0 &&
           bytesIn_ >= static_cast<size_t>(expectedLength_);
  }

  virtual void release() CXX11_OVERRIDE {}

  virtual const std::string& getName() const CXX11_OVERRIDE
  {
    static const std::string name("ErrorPageStreamFilter");
    return name;
  }

  virtual size_t getBytesProcessed() const CXX11_OVERRIDE
  {
    return bytesProcessed_;
  }

private:
  std::unique_ptr<error_page::Writer> writer_;
  int64_t expectedLength_;
  size_t bytesIn_;
  size_t bytesProcessed_;
};

} // namespace aria2

#endif // D_ERROR_PAGE_STREAM_FILTER_H
