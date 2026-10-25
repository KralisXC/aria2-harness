/* <!-- copyright */
/*
 * aria2 - The high speed download utility
 *
 * Copyright (C) 2006 Tatsuhiro Tsujikawa
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
 *
 * In addition, as a special exception, the copyright holders give
 * permission to link the code of portions of this program with the
 * OpenSSL library under certain conditions as described in each
 * individual source file, and distribute linked combinations
 * including the two.
 * You must obey the GNU General Public License in all respects
 * for all of the code used other than OpenSSL.  If you modify
 * file(s) with this exception, you may extend this exception to your
 * version of the file(s), but you are not obligated to do so.  If you
 * do not wish to do so, delete this exception statement from your
 * version.  If you delete this exception statement from all source
 * files in the program, then also delete it here.
 */
/* copyright --> */
#ifndef D_DOWNLOAD_ENGINE_H
#define D_DOWNLOAD_ENGINE_H

#include "common.h"

#include <string>
#include <deque>
#include <map>
#include <vector>
#include <memory>

#include "a2netcompat.h"
#include "TimerA2.h"
#include "a2io.h"
#include "CUIDCounter.h"
#include "FileAllocationMan.h"
#include "CheckIntegrityMan.h"
#include "DNSCache.h"
#ifdef ENABLE_ASYNC_DNS
#  include "AsyncNameResolver.h"
#endif // ENABLE_ASYNC_DNS

namespace aria2 {

class Option;
class RequestGroupMan;
class StatCalc;
class SocketCore;
class CookieStorage;
class AuthConfigFactory;
class Request;
class EventPoll;
class Command;
class FileAllocationAsyncMan;
#ifdef ENABLE_BITTORRENT
class BtRegistry;
#endif // ENABLE_BITTORRENT
#ifdef ENABLE_WEBSOCKET
namespace rpc {
class WebSocketSessionMan;
} // namespace rpc
#endif // ENABLE_WEBSOCKET

namespace util {
namespace security {
class HMAC;
class HMACResult;
} // namespace security
} // namespace util

class DownloadEngine {
private:
  void waitData();

  std::string sessionId_;

  std::unique_ptr<EventPoll> eventPoll_;

  // P0-1: 文件分配的异步调度器。它是 I/O 线程池（AsyncIoExecutor）与跨平台
  // 唤醒管道（WakeupPipe）的唯一持有者 —— 线程池与管道的「停止 → 销毁」次序
  // 由它内部的 shutdown() 统一保证，DownloadEngine 只负责在析构时调用它。
  std::unique_ptr<FileAllocationAsyncMan> asyncMan_;

  // 常驻唤醒命令的裸指针（所有权在 routineCommands_ 里）。仅用于析构/自我
  // 终结时注销它在 EventPoll 上的读事件注册，避免残留悬垂指针。
  Command* wakeupCommand_ = nullptr;

  std::unique_ptr<StatCalc> statCalc_;

  int haltRequested_;

  class SocketPoolEntry {
  private:
    std::shared_ptr<SocketCore> socket_;
    // protocol specific option string
    std::string options_;

    std::chrono::seconds timeout_;

    Timer registeredTime_;

  public:
    SocketPoolEntry(const std::shared_ptr<SocketCore>& socket,
                    const std::string& option, std::chrono::seconds timeout);

    SocketPoolEntry(const std::shared_ptr<SocketCore>& socket,
                    std::chrono::seconds timeout);

    ~SocketPoolEntry();

    bool isTimeout() const;

    const std::shared_ptr<SocketCore>& getSocket() const { return socket_; }

    const std::string& getOptions() const { return options_; }
  };

  // key = IP address:port, value = SocketPoolEntry
  std::multimap<std::string, SocketPoolEntry> socketPool_;

  Timer lastSocketPoolScan_;

  bool noWait_;

  std::chrono::milliseconds refreshInterval_;
  Timer lastRefresh_;

  std::unique_ptr<CookieStorage> cookieStorage_;

#ifdef ENABLE_BITTORRENT
  std::unique_ptr<BtRegistry> btRegistry_;
#endif // ENABLE_BITTORRENT

  CUIDCounter cuidCounter_;

#ifdef HAVE_ARES_ADDR_NODE
  ares_addr_node* asyncDNSServers_;
#endif // HAVE_ARES_ADDR_NODE

  std::unique_ptr<DNSCache> dnsCache_;

  std::unique_ptr<AuthConfigFactory> authConfigFactory_;

#ifdef ENABLE_WEBSOCKET
  std::unique_ptr<rpc::WebSocketSessionMan> webSocketSessionMan_;
#endif // ENABLE_WEBSOCKET

  /**
   * Delegates to StatCalc
   */
  void calculateStatistics();

  void onEndOfRun();

  void afterEachIteration();

  // P0-1: 在主循环最前方调用，消费异步文件分配的结果邮箱（有结果待取时
  // 强制本轮不阻塞，让持有对应 cuid 的 FileAllocationCommand 尽快 tryFinish()）。
  void processAsyncFileAllocation();

  void poolSocket(const std::string& key, const SocketPoolEntry& entry);

  std::multimap<std::string, SocketPoolEntry>::iterator
  findSocketPoolEntry(const std::string& key);

  std::unique_ptr<RequestGroupMan> requestGroupMan_;
  std::unique_ptr<FileAllocationMan> fileAllocationMan_;
  std::unique_ptr<CheckIntegrityMan> checkIntegrityMan_;
  Option* option_;
  // Ensure that Commands are cleaned up before requestGroupMan_ is
  // deleted.
  std::deque<std::unique_ptr<Command>> routineCommands_;
  std::deque<std::unique_ptr<Command>> commands_;

  std::unique_ptr<util::security::HMAC> tokenHMAC_;
  std::unique_ptr<util::security::HMACResult> tokenExpected_;

public:
  DownloadEngine(std::unique_ptr<EventPoll> eventPoll);

  ~DownloadEngine();

  // If oneshot is true, this function returns after one event polling
  // and performing action for them. This function returns 1 when
  // oneshot is true and there are still downloads to be
  // processed. Otherwise, returns 0.
  int run(bool oneshot = false);

  bool addSocketForReadCheck(const std::shared_ptr<SocketCore>& socket,
                             Command* command);
  bool deleteSocketForReadCheck(const std::shared_ptr<SocketCore>& socket,
                                Command* command);
  bool addSocketForWriteCheck(const std::shared_ptr<SocketCore>& socket,
                              Command* command);
  bool deleteSocketForWriteCheck(const std::shared_ptr<SocketCore>& socket,
                                 Command* command);

#ifdef ENABLE_ASYNC_DNS

  bool addNameResolverCheck(const std::shared_ptr<AsyncNameResolver>& resolver,
                            Command* command);
  bool
  deleteNameResolverCheck(const std::shared_ptr<AsyncNameResolver>& resolver,
                          Command* command);
#endif // ENABLE_ASYNC_DNS

  void addCommand(std::vector<std::unique_ptr<Command>> commands);

  void addCommand(std::unique_ptr<Command> command);

  const std::unique_ptr<RequestGroupMan>& getRequestGroupMan() const
  {
    return requestGroupMan_;
  }

  void setRequestGroupMan(std::unique_ptr<RequestGroupMan> rgman);

  const std::unique_ptr<FileAllocationMan>& getFileAllocationMan() const
  {
    return fileAllocationMan_;
  }

  void setFileAllocationMan(std::unique_ptr<FileAllocationMan> faman);

  // P0-1: 异步文件分配调度器。返回值用 const 引用以便与既有 getFileAllocationMan()
  // 保持一致的调用风格（operator-> 仍返回非 const 指针）。
  const std::unique_ptr<FileAllocationAsyncMan>&
  getFileAllocationAsyncMan() const
  {
    return asyncMan_;
  }

  // P0-1: 注销唤醒 fd 的读事件注册。由 WakeupCommand 在自我终结前调用，
  // 避免 EventPoll 的 socketEntries_ 里残留一个已析构的 Command*。幂等。
  void unregisterWakeupCommand();

  const std::unique_ptr<CheckIntegrityMan>& getCheckIntegrityMan() const
  {
    return checkIntegrityMan_;
  }

  void setCheckIntegrityMan(std::unique_ptr<CheckIntegrityMan> ciman);

  Option* getOption() const { return option_; }

  void setOption(Option* op) { option_ = op; }

  void setStatCalc(std::unique_ptr<StatCalc> statCalc);

  bool isHaltRequested() const { return haltRequested_; }

  bool isForceHaltRequested() const { return haltRequested_ >= 2; }

  void requestHalt();

  void requestForceHalt();

  void setNoWait(bool b);

  void addRoutineCommand(std::unique_ptr<Command> command);

  void poolSocket(const std::string& ipaddr, uint16_t port,
                  const std::string& username, const std::string& proxyhost,
                  uint16_t proxyport, const std::shared_ptr<SocketCore>& sock,
                  const std::string& options,
                  std::chrono::seconds timeout = 15_s);

  void poolSocket(const std::shared_ptr<Request>& request,
                  const std::string& username,
                  const std::shared_ptr<Request>& proxyRequest,
                  const std::shared_ptr<SocketCore>& socket,
                  const std::string& options,
                  std::chrono::seconds timeout = 15_s);

  void poolSocket(const std::string& ipaddr, uint16_t port,
                  const std::string& proxyhost, uint16_t proxyport,
                  const std::shared_ptr<SocketCore>& sock,
                  std::chrono::seconds timeout = 15_s);

  void poolSocket(const std::shared_ptr<Request>& request,
                  const std::shared_ptr<Request>& proxyRequest,
                  const std::shared_ptr<SocketCore>& socket,
                  std::chrono::seconds timeout = 15_s);

  std::shared_ptr<SocketCore> popPooledSocket(const std::string& ipaddr,
                                              uint16_t port,
                                              const std::string& proxyhost,
                                              uint16_t proxyport);

  std::shared_ptr<SocketCore>
  popPooledSocket(std::string& options, const std::string& ipaddr,
                  uint16_t port, const std::string& username,
                  const std::string& proxyhost, uint16_t proxyport);

  std::shared_ptr<SocketCore>
  popPooledSocket(const std::vector<std::string>& ipaddrs, uint16_t port);

  std::shared_ptr<SocketCore>
  popPooledSocket(std::string& options, const std::vector<std::string>& ipaddrs,
                  uint16_t port, const std::string& username);

  void evictSocketPool();

  const std::unique_ptr<CookieStorage>& getCookieStorage() const;

#ifdef ENABLE_BITTORRENT
  const std::unique_ptr<BtRegistry>& getBtRegistry() const
  {
    return btRegistry_;
  }
#endif // ENABLE_BITTORRENT

  cuid_t newCUID();

  const std::string& findCachedIPAddress(const std::string& hostname,
                                         uint16_t port) const;

  template <typename OutputIterator>
  void findAllCachedIPAddresses(OutputIterator out, const std::string& hostname,
                                uint16_t port) const
  {
    dnsCache_->findAll(out, hostname, port);
  }

  void cacheIPAddress(const std::string& hostname, const std::string& ipaddr,
                      uint16_t port);

  void markBadIPAddress(const std::string& hostname, const std::string& ipaddr,
                        uint16_t port);

  void removeCachedIPAddress(const std::string& hostname, uint16_t port);

  void setAuthConfigFactory(std::unique_ptr<AuthConfigFactory> factory);

  const std::unique_ptr<AuthConfigFactory>& getAuthConfigFactory() const;

  void setRefreshInterval(std::chrono::milliseconds interval);

  const std::string getSessionId() const { return sessionId_; }

#ifdef HAVE_ARES_ADDR_NODE
  void setAsyncDNSServers(ares_addr_node* asyncDNSServers);

  ares_addr_node* getAsyncDNSServers() const { return asyncDNSServers_; }
#endif // HAVE_ARES_ADDR_NODE

#ifdef ENABLE_WEBSOCKET
  void setWebSocketSessionMan(std::unique_ptr<rpc::WebSocketSessionMan> wsman);
  const std::unique_ptr<rpc::WebSocketSessionMan>&
  getWebSocketSessionMan() const
  {
    return webSocketSessionMan_;
  }
#endif // ENABLE_WEBSOCKET

  bool validateToken(const std::string& token);
};

} // namespace aria2

#endif // D_DOWNLOAD_ENGINE_H
