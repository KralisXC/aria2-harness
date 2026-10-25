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
#include "Http2Stream.h"

#include <cstring>
#include <string>
#include <utility>
#include <vector>

#include "Http2StreamRecvBuffer.h"
#include "SocketCore.h"

namespace aria2 {

Http2Stream::Http2Stream(cuid_t cuid,
                         std::shared_ptr<SocketCore> placeholderSocket)
    : cuid_(cuid),
      recvBuffer_(std::make_shared<Http2StreamRecvBuffer>(
          std::move(placeholderSocket)))
{
  // The buffer's back-reference is installed by Http2Multiplexer right after
  // the stream is wrapped into a shared_ptr (see createStream()), because a
  // shared_ptr to this object does not exist yet inside the constructor.
}

Http2Stream::~Http2Stream() = default;

int32_t Http2Stream::submitRequest(nghttp2_session* session,
                                   const Http2RequestHead& head)
{
  // nghttp2_nv only carries raw name/value pointers, and nghttp2 reads them
  // during nghttp2_submit_request().  Building the array straight from the
  // call sites would bind a temporary std::string to each "const char*"
  // literal argument, whose lifetime ends at the end of the enclosing
  // statement - i.e. every pseudo header name would dangle by the time
  // submit_request() reads it, producing a malformed HEADERS frame.
  // Therefore own the strings first, then take pointers into that stable
  // storage.
  std::vector<std::pair<std::string, std::string>> owned;
  owned.reserve(4 + head.headers.size());
  // Pseudo headers must precede regular headers.
  owned.push_back(std::make_pair(std::string(":method"), head.method));
  owned.push_back(std::make_pair(std::string(":scheme"), head.scheme));
  owned.push_back(std::make_pair(std::string(":authority"), head.authority));
  owned.push_back(std::make_pair(std::string(":path"), head.path));
  for (const auto& header : head.headers) {
    owned.push_back(header);
  }

  std::vector<nghttp2_nv> nva;
  nva.reserve(owned.size());
  for (const auto& kv : owned) {
    nghttp2_nv nv;
    nv.name = reinterpret_cast<uint8_t*>(const_cast<char*>(kv.first.data()));
    nv.namelen = kv.first.size();
    nv.value = reinterpret_cast<uint8_t*>(const_cast<char*>(kv.second.data()));
    nv.valuelen = kv.second.size();
    nv.flags = NGHTTP2_NV_FLAG_NONE;
    nva.push_back(nv);
  }

  int32_t streamId = nghttp2_submit_request(session, nullptr, nva.data(),
                                            nva.size(), nullptr, nullptr);
  if (streamId >= 0) {
    streamId_ = streamId;
  }
  return streamId;
}

void Http2Stream::pushData(const unsigned char* data, size_t len)
{
  if (len == 0) {
    return;
  }
  totalReceived_ += len;

  // Compact away the already staged prefix before appending, so the staging
  // area cannot grow without bound while the head of it is being drained.
  if (stagingOffset_ > 0) {
    staging_.erase(staging_.begin(), staging_.begin() + stagingOffset_);
    stagingOffset_ = 0;
  }
  staging_.insert(staging_.end(), data, data + len);

  refillBuffer();
}

size_t Http2Stream::refillBuffer()
{
  if (stagingOffset_ >= staging_.size()) {
    staging_.clear();
    stagingOffset_ = 0;
    return 0;
  }

  size_t n = recvBuffer_->append(staging_.data() + stagingOffset_,
                                 staging_.size() - stagingOffset_);
  stagingOffset_ += n;
  if (stagingOffset_ >= staging_.size()) {
    staging_.clear();
    stagingOffset_ = 0;
  }
  return n;
}

size_t Http2Stream::getPendingBytes() const
{
  return recvBuffer_->getBufferLength() + (staging_.size() - stagingOffset_);
}

size_t Http2Stream::takeConsumedBytes()
{
  size_t pending = getPendingBytes();
  if (totalReceived_ < pending + credited_) {
    return 0;
  }
  size_t consumed = totalReceived_ - pending - credited_;
  credited_ += consumed;
  return consumed;
}

void Http2Stream::addHeader(const std::string& name, const std::string& value)
{
  headers_.insert(std::make_pair(name, value));
}

std::string Http2Stream::getHeader(const std::string& name) const
{
  auto it = headers_.find(name);
  if (it == headers_.end()) {
    return std::string();
  }
  return it->second;
}

} // namespace aria2
